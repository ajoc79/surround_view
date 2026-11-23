use std::collections::VecDeque;

pub struct Frame {
    pub pts: f64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct VideoInfo {
    pub name: String,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
}

pub enum Event {
    Opened(VideoInfo),
    Frame(Frame),
    Warning(String),
    Failed(String),
    End,
}

pub const QUEUE_LIMIT: usize = 6;

pub fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 2
        && height > 2
        && width <= 4096
        && height <= 4096
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
        && width as u64 * 1080 == height as u64 * 1920
}

pub fn present(queue: &mut VecDeque<Frame>, time: f64) -> Option<Frame> {
    let mut latest = None;
    while queue.front().is_some_and(|frame| frame.pts <= time + 0.001) {
        latest = queue.pop_front();
    }
    latest
}
