// SPDX-License-Identifier: GPL-2.0-only
//! The TS of a bridge, split among the tuners it carries.
//!
//! The splitting follows the stream handlers of the device files of
//! px4_drv, Copyright (c) 2018-2021 nns779.

use std::sync::{Arc, Mutex};

use futures::channel::mpsc;
use tunelith_core::usb::BulkIn;

use crate::it930x::XFER_SIZE;

const TRANSFERS_IN_FLIGHT: usize = 8;
/// The transfers a tuner's stream may fall behind by before its data is
/// dropped.
const BACKLOG: usize = 64;

#[derive(Default)]
struct Streams {
    senders: [Option<mpsc::Sender<Vec<u8>>>; 5],
    running: bool,
}

/// The streams of the tuners on one bridge.
#[derive(Clone)]
pub struct Hub {
    /// Whether the bridge tags the packets of each input by their sync byte,
    /// rather than carrying the one input as is.
    tagged: bool,
    streams: Arc<Mutex<Streams>>,
}

impl Hub {
    pub fn new(tagged: bool) -> Self {
        Self {
            tagged,
            streams: Arc::default(),
        }
    }

    /// Takes the stream of `slot`, and whether the reception is to be
    /// started with [`start`](Self::start).
    pub fn attach(&self, slot: usize) -> (mpsc::Receiver<Vec<u8>>, bool) {
        let (tx, rx) = mpsc::channel(BACKLOG);
        let mut streams = self.streams.lock().unwrap();
        streams.senders[slot] = Some(tx);
        (rx, !std::mem::replace(&mut streams.running, true))
    }

    pub fn detach(&self, slot: usize) {
        self.streams.lock().unwrap().senders[slot] = None;
    }

    /// Gives up on the reception [`attach`](Self::attach) asked to start.
    pub fn abort(&self, slot: usize) {
        let mut streams = self.streams.lock().unwrap();
        streams.senders[slot] = None;
        streams.running = false;
    }

    /// Receives on `queue` until no tuner takes the stream any more.
    pub fn start(&self, queue: impl BulkIn) {
        let hub = self.clone();
        crate::spawn(hub.receive(queue));
    }

    async fn receive(self, mut queue: impl BulkIn) {
        for _ in 0..TRANSFERS_IN_FLIGHT {
            queue.submit(XFER_SIZE);
        }

        let mut carry = Vec::new();
        loop {
            let data = queue.next_complete().await;
            let mut streams = self.streams.lock().unwrap();
            let Ok(data) = data else {
                // ponytail: a failed transfer ends every stream; retry it if
                // devices turn out to fail transiently.
                *streams = Streams::default();
                return;
            };
            queue.submit(XFER_SIZE);

            let mut packets: [Vec<u8>; 5] = Default::default();
            demux(&mut carry, &data, self.tagged, |slot, packet| {
                packets[slot].extend_from_slice(packet)
            });
            for (sender, packets) in streams.senders.iter_mut().zip(packets) {
                if let Some(tx) = sender
                    && !packets.is_empty()
                    && let Err(e) = tx.try_send(packets)
                    && e.is_disconnected()
                {
                    *sender = None;
                }
                // ponytail: a stream that falls BACKLOG transfers behind
                // loses packets silently; report the drop once the daemon can.
            }

            if streams.senders.iter().all(Option::is_none) {
                streams.running = false;
                return;
            }
        }
    }
}

/// Splits the stream of a bridge into the 188-byte packets of each input.
/// Tagged, the sync byte tells the input, 0x17 for the first to 0x57 for the
/// fifth, and is restored to 0x47; untagged, every packet is of the first.
/// `carry` keeps what is left over for the next call.
fn demux(carry: &mut Vec<u8>, data: &[u8], tagged: bool, mut out: impl FnMut(usize, &[u8])) {
    const PACKET: usize = 188;
    // Packets in a row with a sync byte it takes to lock back on the stream.
    const SYNC_COUNT: usize = 4;

    let sync = |byte: u8| {
        if tagged {
            byte & 0x8f == 0x07
        } else {
            byte == 0x47
        }
    };

    carry.extend_from_slice(data);
    let buf = &mut carry[..];
    let mut p = 0;
    while buf.len() - p >= PACKET * SYNC_COUNT {
        if !(0..SYNC_COUNT).all(|k| sync(buf[p + PACKET * k])) {
            p += 1;
            continue;
        }
        while buf.len() - p >= PACKET && sync(buf[p]) {
            let id = if tagged {
                usize::from((buf[p] & 0x70) >> 4)
            } else {
                1
            };
            if (1..=5).contains(&id) {
                buf[p] = 0x47;
                out(id - 1, &buf[p..p + PACKET]);
            }
            p += PACKET;
        }
    }
    carry.drain(..p);
}

#[cfg(test)]
mod tests {
    use super::demux;

    fn packet(sync: u8, fill: u8) -> Vec<u8> {
        let mut p = vec![fill; 188];
        p[0] = sync;
        p
    }

    #[test]
    fn splits_tagged_and_resyncs() {
        let mut stream = vec![0xaa; 5]; // garbage before the first packet
        for i in 0..10u8 {
            stream.extend(packet(((i % 5 + 1) << 4) | 0x07, i));
        }

        let mut carry = Vec::new();
        let mut got: Vec<(usize, u8)> = Vec::new();
        // Cut the stream mid-packet to exercise the carry.
        let (a, b) = stream.split_at(700);
        for part in [a, b] {
            demux(&mut carry, part, true, |id, p| {
                assert_eq!(p[0], 0x47);
                got.push((id, p[1]));
            });
        }

        let expected: Vec<_> = (0..10u8).map(|i| (usize::from(i % 5), i)).collect();
        assert_eq!(got, expected);
        assert!(carry.is_empty());
    }

    #[test]
    fn passes_untagged() {
        let mut stream = vec![0x47; 3];
        for i in 0..6u8 {
            stream.extend(packet(0x47, i));
        }

        let mut carry = Vec::new();
        let mut got = Vec::new();
        demux(&mut carry, &stream, false, |id, p| got.push((id, p[1])));
        let expected: Vec<_> = (0..6u8).map(|i| (0, i)).collect();
        assert_eq!(got, expected);
    }
}
