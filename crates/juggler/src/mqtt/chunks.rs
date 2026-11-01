//! [`ChunkAssembler`] — joins a received MQTT message that the client delivered
//! in chunks (esp-mqtt does this when a message exceeds its receive buffer).
//!
//! The first chunk carries the topic and the total size; follow-up chunks carry
//! their offset only. The assembler hands out a message once all bytes arrived,
//! refuses totals above its cap (the size comes from the network), and reports
//! chunks that do not continue the message in progress.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One received piece of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// A whole message in one piece.
    Complete {
        /// Topic.
        topic: &'a str,
        /// Body.
        data: &'a [u8],
    },
    /// The first piece of a longer message.
    First {
        /// Topic.
        topic: &'a str,
        /// The first bytes of the body.
        data: &'a [u8],
        /// Size of the whole body.
        total: usize,
    },
    /// A later piece.
    Next {
        /// These bytes.
        data: &'a [u8],
        /// Where they start in the body.
        offset: usize,
        /// Size of the whole body.
        total: usize,
    },
}

/// What [`ChunkAssembler::feed`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assembled {
    /// A whole message.
    Message {
        /// Topic.
        topic: String,
        /// Body.
        payload: Vec<u8>,
    },
    /// More chunks are needed.
    Pending,
    /// The announced size exceeds the cap; the message's chunks are skipped.
    TooLarge,
    /// The chunk does not continue the message in progress; it is discarded.
    OutOfOrder,
}

#[derive(Debug)]
enum Progress {
    Idle,
    Collecting {
        topic: String,
        payload: Vec<u8>,
        total: usize,
    },
    /// Skipping the remaining chunks of a message that was too large.
    Skipping {
        total: usize,
    },
}

/// Joins chunked messages, one at a time, up to `max_len` bytes each.
#[derive(Debug)]
pub struct ChunkAssembler {
    max_len: usize,
    progress: Progress,
    abandoned: u32,
}

impl ChunkAssembler {
    /// An assembler for bodies up to `max_len` bytes.
    pub const fn new(max_len: usize) -> Self {
        Self {
            max_len,
            progress: Progress::Idle,
            abandoned: 0,
        }
    }

    /// Messages left incomplete because a new one started before they finished.
    pub fn abandoned(&self) -> u32 {
        self.abandoned
    }

    /// Feeds one chunk.
    pub fn feed(&mut self, chunk: Chunk<'_>) -> Assembled {
        match chunk {
            Chunk::Complete { topic, data } => {
                self.abandon();
                if data.len() > self.max_len {
                    return Assembled::TooLarge;
                }
                Assembled::Message {
                    topic: topic.to_string(),
                    payload: data.to_vec(),
                }
            }
            Chunk::First { topic, data, total } => {
                self.abandon();
                if total > self.max_len {
                    self.progress = Progress::Skipping { total };
                    return Assembled::TooLarge;
                }
                let mut payload = Vec::with_capacity(total);
                payload.extend_from_slice(data);
                self.progress = Progress::Collecting {
                    topic: topic.to_string(),
                    payload,
                    total,
                };
                self.finish_if_complete()
            }
            Chunk::Next {
                data,
                offset,
                total,
            } => match &mut self.progress {
                Progress::Collecting {
                    payload,
                    total: expected,
                    ..
                } if *expected == total
                    && offset == payload.len()
                    && offset + data.len() <= total =>
                {
                    payload.extend_from_slice(data);
                    self.finish_if_complete()
                }
                Progress::Skipping { total: skipped } if *skipped == total => {
                    if offset + data.len() >= total {
                        self.progress = Progress::Idle;
                    }
                    Assembled::Pending
                }
                _ => {
                    self.abandon();
                    Assembled::OutOfOrder
                }
            },
        }
    }

    fn finish_if_complete(&mut self) -> Assembled {
        match &self.progress {
            Progress::Collecting { payload, total, .. } if payload.len() == *total => {
                match core::mem::replace(&mut self.progress, Progress::Idle) {
                    Progress::Collecting { topic, payload, .. } => {
                        Assembled::Message { topic, payload }
                    }
                    _ => unreachable!("matched Collecting above"),
                }
            }
            _ => Assembled::Pending,
        }
    }

    /// Drops a message in progress, counting it if it had started collecting.
    fn abandon(&mut self) {
        if matches!(self.progress, Progress::Collecting { .. }) {
            self.abandoned = self.abandoned.wrapping_add(1);
        }
        self.progress = Progress::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: &str, payload: &[u8]) -> Assembled {
        Assembled::Message {
            topic: topic.to_string(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn a_complete_chunk_is_a_message() {
        let mut a = ChunkAssembler::new(64);
        let out = a.feed(Chunk::Complete {
            topic: "t",
            data: b"hello",
        });
        assert_eq!(out, message("t", b"hello"));
    }

    #[test]
    fn chunks_are_joined_into_one_message() {
        let mut a = ChunkAssembler::new(64);
        assert_eq!(
            a.feed(Chunk::First {
                topic: "t",
                data: b"hel",
                total: 8
            }),
            Assembled::Pending
        );
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"lo ",
                offset: 3,
                total: 8
            }),
            Assembled::Pending
        );
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"you",
                offset: 6,
                total: 8
            }),
            Assembled::OutOfOrder,
            "3 + 3 + 3 bytes overrun the announced 8"
        );
        a.feed(Chunk::First {
            topic: "t",
            data: b"hel",
            total: 8,
        });
        a.feed(Chunk::Next {
            data: b"lo ",
            offset: 3,
            total: 8,
        });
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"yo",
                offset: 6,
                total: 8
            }),
            message("t", b"hello yo")
        );
    }

    #[test]
    fn a_message_above_the_cap_is_skipped_to_its_end() {
        let mut a = ChunkAssembler::new(4);
        assert_eq!(
            a.feed(Chunk::First {
                topic: "big",
                data: b"abc",
                total: 6
            }),
            Assembled::TooLarge
        );
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"def",
                offset: 3,
                total: 6
            }),
            Assembled::Pending,
            "the rest of a skipped message is silently consumed"
        );
        assert_eq!(
            a.feed(Chunk::Complete {
                topic: "t",
                data: b"ok"
            }),
            message("t", b"ok")
        );
        assert_eq!(a.abandoned(), 0);
    }

    #[test]
    fn a_complete_chunk_above_the_cap_is_too_large() {
        let mut a = ChunkAssembler::new(2);
        assert_eq!(
            a.feed(Chunk::Complete {
                topic: "t",
                data: b"abc"
            }),
            Assembled::TooLarge
        );
    }

    #[test]
    fn a_follow_up_without_a_first_chunk_is_out_of_order() {
        let mut a = ChunkAssembler::new(64);
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"x",
                offset: 3,
                total: 8
            }),
            Assembled::OutOfOrder
        );
    }

    #[test]
    fn a_gap_in_the_offsets_drops_the_message() {
        let mut a = ChunkAssembler::new(64);
        a.feed(Chunk::First {
            topic: "t",
            data: b"ab",
            total: 6,
        });
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"ef",
                offset: 4,
                total: 6
            }),
            Assembled::OutOfOrder
        );
        assert_eq!(a.abandoned(), 1);
        assert_eq!(
            a.feed(Chunk::Next {
                data: b"cd",
                offset: 2,
                total: 6
            }),
            Assembled::OutOfOrder,
            "after a gap nothing is in progress any more"
        );
    }

    #[test]
    fn a_new_message_abandons_an_unfinished_one() {
        let mut a = ChunkAssembler::new(64);
        a.feed(Chunk::First {
            topic: "t",
            data: b"ab",
            total: 6,
        });
        assert_eq!(
            a.feed(Chunk::Complete {
                topic: "u",
                data: b"new"
            }),
            message("u", b"new")
        );
        assert_eq!(a.abandoned(), 1);
    }
}
