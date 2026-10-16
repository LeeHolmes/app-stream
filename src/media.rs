use std::{
    io::Read,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::sync::{broadcast, watch};

const MAX_ACCESS_UNIT: usize = 32 * 1024 * 1024;
const RTP_PAYLOAD: usize = 1200;

#[derive(Clone, Debug)]
pub struct Parameters {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

#[derive(Debug)]
pub struct AccessUnit {
    pub nals: Vec<Vec<u8>>,
    pub timestamp: u32,
    pub keyframe: bool,
}

#[derive(Clone)]
pub struct Media {
    pub frames: broadcast::Sender<Arc<AccessUnit>>,
    pub parameters: watch::Receiver<Option<Parameters>>,
    pub activity: Arc<Activity>,
}

#[derive(Default)]
pub struct Activity {
    viewers: Mutex<usize>,
    changed: Condvar,
    epoch: AtomicU64,
}

impl Activity {
    pub fn playing(self: &Arc<Self>) -> Playing {
        let mut viewers = self.viewers.lock().expect("Viewer count mutex poisoned");
        if *viewers == 0 {
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
        *viewers += 1;
        drop(viewers);
        self.changed.notify_all();
        Playing(self.clone())
    }

    pub fn wait(&self, duration: Duration) -> bool {
        let viewers = self.viewers.lock().expect("Viewer count mutex poisoned");
        let viewers = if *viewers == 0 && !duration.is_zero() {
            self.changed
                .wait_timeout(viewers, duration)
                .expect("Viewer count mutex poisoned")
                .0
        } else {
            viewers
        };
        *viewers > 0
    }

    pub fn wake(&self) {
        self.changed.notify_all();
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn viewer_count(&self) -> usize {
        *self.viewers.lock().expect("Viewer count mutex poisoned")
    }
}

pub struct Playing(Arc<Activity>);

impl Drop for Playing {
    fn drop(&mut self) {
        *self.0.viewers.lock().expect("Viewer count mutex poisoned") -= 1;
        self.0.changed.notify_all();
    }
}

pub fn channel() -> (Media, watch::Sender<Option<Parameters>>) {
    let (frames, _) = broadcast::channel(32);
    let (parameters_tx, parameters) = watch::channel(None);
    (
        Media {
            frames,
            parameters,
            activity: Arc::default(),
        },
        parameters_tx,
    )
}

// AUDs delimit complete frames; x264 is configured to emit one before every frame.
pub fn read_h264(
    mut input: impl Read,
    fps: u32,
    media: Media,
    parameters: watch::Sender<Option<Parameters>>,
) -> Result<()> {
    let mut parser = AnnexB::default();
    let mut buffer = [0; 64 * 1024];
    let mut unit: Vec<Vec<u8>> = Vec::new();
    let mut sps = Vec::new();
    let mut pps = Vec::new();
    let mut index = 0u64;
    let mut epoch = media.activity.epoch();
    loop {
        let count = input
            .read(&mut buffer)
            .context("Reading FFmpeg H.264 output")?;
        let nals = parser.push(&buffer[..count], count == 0)?;
        for nal in nals {
            let kind = nal[0] & 0x1f;
            if kind == 9 {
                publish(&mut unit, &media, &mut index, fps, epoch);
                epoch = media.activity.epoch();
            }
            match kind {
                7 => sps.clone_from(&nal),
                8 => pps.clone_from(&nal),
                _ => {}
            }
            if matches!(kind, 7 | 8) && !sps.is_empty() && !pps.is_empty() {
                parameters.send_replace(Some(Parameters {
                    sps: sps.clone(),
                    pps: pps.clone(),
                }));
            }
            unit.push(nal);
            if unit.iter().map(Vec::len).sum::<usize>() > MAX_ACCESS_UNIT {
                bail!("FFmpeg access unit exceeds 32 MiB");
            }
        }
        if count == 0 {
            publish(&mut unit, &media, &mut index, fps, epoch);
            bail!("FFmpeg closed its output unexpectedly");
        }
    }
}

fn publish(unit: &mut Vec<Vec<u8>>, media: &Media, index: &mut u64, fps: u32, epoch: u64) {
    // A frame buffered before idle must not become a stale first frame on reconnect.
    if epoch != media.activity.epoch() || !unit.iter().any(|nal| matches!(nal[0] & 0x1f, 1 | 5)) {
        unit.clear();
        return;
    }
    let keyframe = unit.iter().any(|nal| nal[0] & 0x1f == 5);
    let frame = AccessUnit {
        nals: std::mem::take(unit),
        timestamp: ((*index * 90_000) / u64::from(fps)) as u32,
        keyframe,
    };
    *index += 1;
    // No viewers during the brief parameter-set warmup is normal.
    let _ = media.frames.send(Arc::new(frame));
}

#[derive(Default)]
struct AnnexB {
    pending: Vec<u8>,
}

impl AnnexB {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_ACCESS_UNIT {
            bail!("FFmpeg NAL exceeds 32 MiB");
        }
        let mut nals = Vec::new();
        let mut consumed = 0;
        let mut start = None;
        let mut i = 0;
        while i + 3 <= self.pending.len() {
            let prefix = if self.pending[i..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if self.pending[i..].starts_with(&[0, 0, 1]) {
                3
            } else {
                i += 1;
                continue;
            };
            if let Some(begin) = start
                && begin < i
            {
                nals.push(self.pending[begin..i].to_vec());
            }
            start = Some(i + prefix);
            consumed = i;
            i += prefix;
        }
        if eof {
            if let Some(begin) = start
                && begin < self.pending.len()
            {
                nals.push(self.pending[begin..].to_vec());
            }
            self.pending.clear();
        } else if start.is_some() {
            self.pending.drain(..consumed);
        }
        Ok(nals)
    }
}

pub fn rtp_packets(unit: &AccessUnit, sequence: &mut u16, ssrc: u32) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    for (i, nal) in unit.nals.iter().enumerate() {
        let last = i + 1 == unit.nals.len();
        if nal.len() <= RTP_PAYLOAD {
            packets.push(rtp(nal, last, unit.timestamp, sequence, ssrc));
        } else {
            let chunks = nal[1..].chunks(RTP_PAYLOAD - 2);
            let count = chunks.len();
            for (j, chunk) in chunks.enumerate() {
                let mut payload = Vec::with_capacity(chunk.len() + 2);
                payload.push((nal[0] & 0xe0) | 28);
                payload.push(
                    (nal[0] & 0x1f)
                        | if j == 0 { 0x80 } else { 0 }
                        | if j + 1 == count { 0x40 } else { 0 },
                );
                payload.extend_from_slice(chunk);
                packets.push(rtp(
                    &payload,
                    last && j + 1 == count,
                    unit.timestamp,
                    sequence,
                    ssrc,
                ));
            }
        }
    }
    packets
}

fn rtp(payload: &[u8], marker: bool, timestamp: u32, sequence: &mut u16, ssrc: u32) -> Vec<u8> {
    let mut packet = vec![0x80, 96 | if marker { 0x80 } else { 0 }];
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(payload);
    *sequence = sequence.wrapping_add(1);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_counts_viewers_and_only_starts_new_epoch_after_idle() {
        let activity = Arc::new(Activity::default());
        assert!(!activity.wait(Duration::ZERO));
        let first = activity.playing();
        let epoch = activity.epoch();
        let second = activity.playing();
        assert!(activity.wait(Duration::ZERO));
        assert_eq!(activity.epoch(), epoch);
        drop(first);
        assert!(activity.wait(Duration::ZERO));
        drop(second);
        assert!(!activity.wait(Duration::ZERO));
        let _reconnected = activity.playing();
        assert_eq!(activity.epoch(), epoch + 1);
    }

    #[test]
    fn idle_wait_blocks_and_play_wakes_it() {
        let activity = Arc::new(Activity::default());
        let worker_activity = activity.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, result) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            finished
                .send(worker_activity.wait(Duration::from_secs(5)))
                .unwrap();
        });
        ready.recv().unwrap();
        assert!(result.recv_timeout(Duration::from_millis(100)).is_err());
        let _playing = activity.playing();
        assert!(result.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn frames_buffered_before_idle_are_not_published_on_resume() {
        let (media, _parameters) = channel();
        let mut receiver = media.frames.subscribe();
        let epoch = media.activity.epoch();
        let _playing = media.activity.playing();
        let mut old_frame = vec![vec![0x65, 11]];
        let mut index = 0;
        publish(&mut old_frame, &media, &mut index, 30, epoch);
        assert!(receiver.try_recv().is_err());
        let mut fresh_frame = vec![vec![0x65, 22]];
        publish(
            &mut fresh_frame,
            &media,
            &mut index,
            30,
            media.activity.epoch(),
        );
        assert_eq!(receiver.try_recv().unwrap().nals, [vec![0x65, 22]]);
    }

    #[test]
    fn annex_b_handles_split_start_codes() {
        let mut parser = AnnexB::default();
        assert!(parser.push(&[0, 0], false).unwrap().is_empty());
        assert!(
            parser
                .push(&[0, 1, 0x67, 42, 0, 0], false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parser.push(&[1, 0x68, 9, 0, 0, 0, 1, 0x65], false).unwrap(),
            vec![vec![0x67, 42], vec![0x68, 9]]
        );
        assert_eq!(parser.push(&[], true).unwrap(), vec![vec![0x65]]);
    }

    #[test]
    fn fragments_have_correct_headers_and_marker() {
        let nal = [vec![0x65], vec![42; 3000]].concat();
        let frame = AccessUnit {
            nals: vec![vec![0x67, 1], nal.clone()],
            timestamp: 123,
            keyframe: true,
        };
        let mut sequence = u16::MAX;
        let packets = rtp_packets(&frame, &mut sequence, 7);
        assert_eq!(packets.len(), 4);
        assert_eq!(&packets[0][2..4], &u16::MAX.to_be_bytes());
        assert_eq!(&packets[1][2..4], &0u16.to_be_bytes());
        assert_eq!(packets[1][12], 0x7c);
        assert_eq!(packets[1][13], 0x85);
        assert_eq!(packets[3][13], 0x45);
        assert!(packets[..3].iter().all(|p| p[1] & 0x80 == 0));
        assert_ne!(packets[3][1] & 0x80, 0);
        let reconstructed: Vec<u8> = std::iter::once(0x65)
            .chain(packets[1..].iter().flat_map(|p| p[14..].iter().copied()))
            .collect();
        assert_eq!(reconstructed, nal);
    }

    #[test]
    fn reader_publishes_complete_frames_and_parameter_sets() {
        let stream = [
            &[0, 0, 0, 1, 0x09, 0xf0][..],
            &[0, 0, 0, 1, 0x67, 0x42, 0, 0x1f],
            &[0, 0, 1, 0x68, 0xce],
            &[0, 0, 1, 0x65, 11],
            &[0, 0, 1, 0x09, 0xf0],
            &[0, 0, 1, 0x41, 22],
        ]
        .concat();
        let (media, parameters) = channel();
        let mut receiver = media.frames.subscribe();
        assert!(read_h264(std::io::Cursor::new(stream), 30, media.clone(), parameters).is_err());
        let first = receiver.try_recv().unwrap();
        let second = receiver.try_recv().unwrap();
        assert!(first.keyframe);
        assert!(!second.keyframe);
        assert_eq!(first.timestamp, 0);
        assert_eq!(second.timestamp, 3000);
        assert_eq!(first.nals.last().unwrap(), &[0x65, 11]);
        assert_eq!(second.nals.last().unwrap(), &[0x41, 22]);
        let parameters = media.parameters.borrow();
        assert_eq!(parameters.as_ref().unwrap().sps, [0x67, 0x42, 0, 0x1f]);
        assert_eq!(parameters.as_ref().unwrap().pps, [0x68, 0xce]);
    }
}
