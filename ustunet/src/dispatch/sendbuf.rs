use std::collections::VecDeque;
use std::iter::FromIterator;

const BUFSIZE: usize = 2048;
const NUM_BUFFS: usize =4;
/// Buffer packets to be written to device
pub(crate) struct SendingBuffer {
    /// with data
    packets: VecDeque<Vec<u8>>,
    bufs: Vec<Vec<u8>>,
}

impl SendingBuffer {
    pub(crate) fn new() -> Self {
        SendingBuffer {
            packets: VecDeque::new(),
            bufs: Vec::from_iter(std::iter::repeat(vec![]).take(NUM_BUFFS)),
        }
    }
    pub(crate) fn pop_packet(&mut self) -> Option<Vec<u8>> {
        self.packets.pop_front()
    }
    pub(crate) fn first(&self) -> Option<&[u8]> {
        self.packets.front().map(|v| &v[..])
    }
    pub(crate) fn pop_front(&mut self) {
        let f = self.packets.pop_front().unwrap();
        self.bufs.push(f);
     //   error!("num bufs {}", self.bufs.len());
    }
    pub(crate) fn get_buf(&mut self) -> Option<Vec<u8>> {
        if self.bufs.is_empty() { return None}
        let mut b = self.bufs.remove(self.bufs.len()-1);
        b.clear();
        Some(b)
    }
    pub(crate) fn get_mut(&mut self) -> Option<&mut Vec<u8>> {
        if self.bufs.is_empty() {
           // error!("no bufs");
        }
        let b = self.bufs.last_mut()?;
        b.clear();
        Some(b)
    }
    pub(crate) fn queue_buf(&mut self, b: Vec<u8>) {
        if b.is_empty()
        {
            self.bufs.push(b)
        } else {
            self.packets.push_back(b);
        }
    }
    pub(crate) fn queue_written(&mut self) {
        let b = self.bufs.remove(self.bufs.len()-1);
        if b.is_empty()
        {
            self.bufs.push(b)
        } else {
            self.packets.push_back(b);
        }

    }

}
