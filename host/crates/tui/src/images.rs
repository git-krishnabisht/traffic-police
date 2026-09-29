//! Inline images through kitty, iTerm2 or sixel graphics, with half-blocks as the fallback
//! (ARCHITECTURE.md §5.8). Protocols are built once per (image, cell size) and cached.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use image::DynamicImage;
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::picker::Picker;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

pub struct Images {
    picker: Picker,
    cache: HashMap<(usize, u16, u16), Protocol>,
    pub label: &'static str,
}

impl Images {
    /// Half-blocks only (tests, `--dump-frame`, tmux by default).
    pub fn halfblocks() -> Self {
        Images { picker: Picker::halfblocks(), cache: HashMap::new(), label: "half-blocks" }
    }

    /// Ask the terminal which graphics protocol it speaks. Call after entering the alternate
    /// screen and before reading input events (the probe reads stdin).
    pub fn detect() -> Self {
        let in_tmux = std::env::var_os("TMUX").is_some() || std::env::var("TERM").is_ok_and(|t| t.starts_with("tmux"));
        if in_tmux && std::env::var("TRAFFIC_POLICE_IMAGES").map_or(true, |v| v != "auto") {
            // the probe would switch on tmux's allow-passthrough as a side effect
            return Images::halfblocks();
        }
        let opts = QueryStdioOptions { timeout: Duration::from_millis(300), ..Default::default() };
        match Picker::from_query_stdio_with_options(opts) {
            Ok(p) => {
                let label = match p.protocol_type() {
                    ratatui_image::picker::ProtocolType::Halfblocks => "half-blocks",
                    ratatui_image::picker::ProtocolType::Sixel => "sixel",
                    ratatui_image::picker::ProtocolType::Kitty => "kitty",
                    ratatui_image::picker::ProtocolType::Iterm2 => "iTerm2",
                };
                Images { picker: p, cache: HashMap::new(), label }
            }
            Err(_) => Images::halfblocks(),
        }
    }

    /// Draw `img` fitted into `area`.
    pub fn render(&mut self, img: &Arc<DynamicImage>, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let key = (Arc::as_ptr(img) as usize, area.width, area.height);
        if !self.cache.contains_key(&key) {
            if self.cache.len() > 16 {
                self.cache.clear();
            }
            match self.picker.new_protocol((**img).clone(), Size::new(area.width, area.height), Resize::Fit(None)) {
                Ok(p) => {
                    self.cache.insert(key, p);
                }
                Err(_) => return,
            }
        }
        if let Some(p) = self.cache.get(&key) {
            Image::new(p).render(area, buf);
        }
    }
}
