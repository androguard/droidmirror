//! Two-row toolbar bitmap: capture on top, navigation underneath.

pub struct ToolbarState {
    pub gif: bool,
    pub mp4: bool,
}

pub fn paint(width: u32, height: u32, state: ToolbarState) -> Vec<u8> {
    let w = width.max(1);
    let h = height.max(1);
    let mut px = vec![0u8; (w * h * 4) as usize];
    fill(&mut px, w, 0, 0, w, h, [18, 18, 20, 255]);
    let row_h = h / 2;
    let gap = (h / 40).max(1);
    draw_row(
        &mut px,
        w,
        gap,
        gap,
        w - gap * 2,
        row_h.saturating_sub(gap),
        &["PNG", "GIF", "MP4"],
        &[false, state.gif, state.mp4],
    );
    draw_row(
        &mut px,
        w,
        gap,
        row_h,
        w - gap * 2,
        h - row_h - gap,
        &["Back", "Home", "Apps", "Power", "Vol-", "Vol+"],
        &[false; 6],
    );
    px
}

fn draw_row(px: &mut [u8], stride: u32, x: u32, y: u32, w: u32, h: u32, labels: &[&str], hot: &[bool]) {
    if w == 0 || h == 0 || labels.is_empty() {
        return;
    }
    let n = labels.len() as u32;
    let slot = w / n;
    for (i, label) in labels.iter().enumerate() {
        let sx = x + slot * i as u32;
        let sw = if i + 1 == labels.len() { w - slot * i as u32 } else { slot.saturating_sub(2) };
        let bg = if hot.get(i).copied().unwrap_or(false) {
            [150, 32, 36, 255]
        } else {
            [36, 36, 42, 255]
        };
        fill(px, stride, sx, y, sw, h, bg);
        let scale = ((h / 10).clamp(2, 6)) as i32;
        blit_text(px, stride, sx, y, sw, h, label, scale, [242, 242, 244, 255]);
    }
}

fn fill(px: &mut [u8], stride: u32, x: u32, y: u32, w: u32, h: u32, rgba: [u8; 4]) {
    for row in y..y.saturating_add(h).min(px.len() as u32 / (stride * 4).max(1)) {
        for col in x..x.saturating_add(w).min(stride) {
            put(px, stride, col, row, rgba);
        }
    }
}

fn blit_text(px: &mut [u8], stride: u32, x: u32, y: u32, w: u32, h: u32, text: &str, scale: i32, rgba: [u8; 4]) {
    let scale = scale.max(1) as u32;
    let gap = scale;
    let text_w = text.chars().count() as u32 * (5 * scale + gap);
    let text_h = 7 * scale;
    let mut cx = x + w.saturating_sub(text_w) / 2;
    let cy = y + h.saturating_sub(text_h) / 2;
    for ch in text.chars() {
        let glyph = glyph(ch);
        for (gy, row) in glyph.iter().enumerate() {
            for gx in 0..5 {
                if row & (1 << (4 - gx)) == 0 {
                    continue;
                }
                fill(
                    px,
                    stride,
                    cx + gx as u32 * scale,
                    cy + gy as u32 * scale,
                    scale,
                    scale,
                    rgba,
                );
            }
        }
        cx += 5 * scale + gap;
    }
}

fn put(px: &mut [u8], stride: u32, x: u32, y: u32, rgba: [u8; 4]) {
    let i = (y * stride + x) as usize * 4;
    if i + 4 <= px.len() {
        px[i..i + 4].copy_from_slice(&rgba);
    }
}

/// 5×7 glyphs, bit 4 is the leftmost pixel.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        '4' => [0x04, 0x04, 0x14, 0x1F, 0x04, 0x04, 0x04],
        '+' => [0x04, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x04],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_has_label_pixels() {
        let px = paint(300, 80, ToolbarState { gif: true, mp4: false });
        assert_eq!(px.len(), 300 * 80 * 4);
        let ink = px.chunks(4).filter(|p| p[0] > 200).count();
        assert!(ink > 50, "expected label pixels, got {ink}");
    }
}
