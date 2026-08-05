//! Optional non-blocking JSON Lines playback history.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    path::Path,
    sync::{Arc, Mutex},
    thread,
};

use crossbeam_channel::{Sender, TrySendError};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{sync::oneshot, task::JoinHandle};
use uuid::Uuid;

use crate::playback::{LifecycleEvent, PlaybackState};

const WRITER_QUEUE_ITEMS: usize = 1_024;

#[derive(Clone, Debug)]
pub(crate) struct HistoryMetadata {
    pub tool: &'static str,
    pub source_kind: &'static str,
    pub preset_id: Option<String>,
    pub gain: f64,
    pub concurrency: &'static str,
    pub output_target: String,
    pub spoken_text: Option<String>,
}

#[derive(Serialize)]
struct HistoryRecord {
    timestamp: String,
    playback_id: String,
    state: &'static str,
    tool: &'static str,
    source_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    preset_id: Option<String>,
    gain: f64,
    concurrency: &'static str,
    output_target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    spoken_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
}

struct HistoryInner {
    pending: Arc<Mutex<HashMap<Uuid, HistoryMetadata>>>,
    writer_tx: Mutex<Option<Sender<HistoryRecord>>>,
    stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    bridge: Mutex<Option<JoinHandle<()>>>,
    writer: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct HistoryRecorder {
    inner: Arc<HistoryInner>,
}

impl HistoryRecorder {
    pub(crate) fn start(
        path: &Path,
        mut events: tokio::sync::broadcast::Receiver<LifecycleEvent>,
    ) -> io::Result<Self> {
        let file = open_history(path)?;
        let (writer_tx, writer_rx) = crossbeam_channel::bounded(WRITER_QUEUE_ITEMS);
        let writer = thread::Builder::new()
            .name("agent-speak-history".to_owned())
            .spawn(move || writer_loop(file, writer_rx))?;
        let pending = Arc::new(Mutex::new(HashMap::<Uuid, HistoryMetadata>::new()));
        let bridge_pending = pending.clone();
        let bridge_writer = writer_tx.clone();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let bridge = tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => forward(event, &bridge_pending, &bridge_writer),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "playback history dropped lifecycle events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    _ = &mut stop_rx => {
                        while let Ok(event) = events.try_recv() {
                            forward(event, &bridge_pending, &bridge_writer);
                        }
                        break;
                    }
                }
            }
        });

        Ok(Self {
            inner: Arc::new(HistoryInner {
                pending,
                writer_tx: Mutex::new(Some(writer_tx)),
                stop_tx: Mutex::new(Some(stop_tx)),
                bridge: Mutex::new(Some(bridge)),
                writer: Mutex::new(Some(writer)),
            }),
        })
    }

    pub(crate) fn track(&self, playback_id: Uuid, metadata: HistoryMetadata) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(playback_id, metadata);
    }

    pub(crate) fn forget(&self, playback_id: Uuid) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&playback_id);
    }

    pub(crate) async fn shutdown(&self) {
        let stop = self
            .inner
            .stop_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let bridge = self
            .inner
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(bridge) = bridge {
            let _ = bridge.await;
        }
        self.inner
            .writer_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let writer = self
            .inner
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(writer) = writer {
            let _ = tokio::task::spawn_blocking(move || writer.join()).await;
        }
    }
}

fn open_history(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn writer_loop(file: File, records: crossbeam_channel::Receiver<HistoryRecord>) {
    let mut writer = BufWriter::new(file);
    for record in records {
        let result = serde_json::to_writer(&mut writer, &record)
            .and_then(|()| writer.write_all(b"\n").map_err(serde_json::Error::io))
            .and_then(|()| writer.flush().map_err(serde_json::Error::io));
        if let Err(error) = result {
            tracing::error!(%error, "could not append playback history");
        }
    }
}

fn forward(
    event: LifecycleEvent,
    pending: &Mutex<HashMap<Uuid, HistoryMetadata>>,
    writer: &Sender<HistoryRecord>,
) {
    let terminal = matches!(
        event.state,
        PlaybackState::Completed | PlaybackState::Interrupted | PlaybackState::Failed
    );
    let metadata = {
        let mut pending = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if terminal {
            pending.remove(&event.playback_id)
        } else {
            pending.get(&event.playback_id).cloned()
        }
    };
    let Some(metadata) = metadata else {
        return;
    };
    let record = HistoryRecord {
        timestamp: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".to_owned()),
        playback_id: event.playback_id.to_string(),
        state: state_name(event.state),
        tool: metadata.tool,
        source_kind: metadata.source_kind,
        preset_id: metadata.preset_id,
        gain: metadata.gain,
        concurrency: metadata.concurrency,
        output_target: metadata.output_target,
        spoken_text: (event.state == PlaybackState::Accepted)
            .then_some(metadata.spoken_text)
            .flatten(),
        error_code: event.error.map(|_| "playback_unavailable"),
    };
    match writer.try_send(record) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            tracing::warn!("playback history writer queue is full; dropping record");
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::warn!("playback history writer is unavailable; dropping record");
        }
    }
}

fn state_name(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Accepted => "accepted",
        PlaybackState::Playing => "playing",
        PlaybackState::Completed => "completed",
        PlaybackState::Interrupted => "interrupted",
        PlaybackState::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::broadcast;

    use super::*;

    #[tokio::test]
    async fn writes_sanitized_lifecycle_json_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        let (events, receiver) = broadcast::channel(8);
        let history = HistoryRecorder::start(&path, receiver).unwrap();
        let id = Uuid::new_v4();
        history.track(
            id,
            HistoryMetadata {
                tool: "speak_text",
                source_kind: "arbitrary_text",
                preset_id: None,
                gain: 0.4,
                concurrency: "enqueue",
                output_target: "system".to_owned(),
                spoken_text: None,
            },
        );
        for state in [
            PlaybackState::Accepted,
            PlaybackState::Playing,
            PlaybackState::Completed,
        ] {
            events
                .send(LifecycleEvent {
                    playback_id: id,
                    state,
                    error: None,
                })
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        history.shutdown().await;

        let records = std::fs::read_to_string(path).unwrap();
        let lines: Vec<_> = records.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"state\":\"accepted\""));
        assert!(lines[0].contains("\"output_target\":\"system\""));
        assert!(lines[2].contains("\"state\":\"completed\""));
        assert!(!records.contains("spoken_text"));
    }
}
