//! Software framebuffer pixel adapter, DRM dumb-buffer row blit, cursor overlay, and evdev key mapping.
//!
//! Kept free of DRM/libinput FFI handles so it is always compiled and unit-tested
//! across both `winit` and bare-metal `kms` builds.

use slint::platform::software_renderer::{PremultipliedRgbaColor, TargetPixel};
use slint::platform::Key;
use slint::SharedString;

/// 32-bit `XRGB8888` pixel (`0xFF_RR_GG_BB` in native little-endian layout for DRM dumb buffers).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Xrgb8888Pixel(pub u32);

impl TargetPixel for Xrgb8888Pixel {
    #[inline]
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        if color.alpha == u8::MAX {
            *self = Self::from_rgb(color.red, color.green, color.blue);
            return;
        }
        if color.alpha == 0 {
            return;
        }
        let inv_a = u16::from(u8::MAX - color.alpha);
        let r = (((self.0 >> 16) & 0xff) as u16 * inv_a / 255) as u8;
        let g = (((self.0 >> 8) & 0xff) as u16 * inv_a / 255) as u8;
        let b = ((self.0 & 0xff) as u16 * inv_a / 255) as u8;
        self.0 = 0xFF00_0000
            | (u32::from(r.saturating_add(color.red)) << 16)
            | (u32::from(g.saturating_add(color.green)) << 8)
            | u32::from(b.saturating_add(color.blue));
    }

    #[inline]
    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xFF00_0000 | (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue))
    }

    #[inline]
    fn background() -> Self {
        Self(0xFF09_0D16)
    }
}

/// Copy `width * height` `Xrgb8888Pixel` pixels into a mapped DRM dumb buffer obeying row `pitch`.
pub fn copy_xrgb8888_to_dumb_buffer(
    src: &[Xrgb8888Pixel],
    width: usize,
    height: usize,
    dst: &mut [u8],
    pitch: usize,
) -> Result<(), String> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| "framebuffer width overflow".to_string())?;
    if pitch < row_bytes {
        return Err(format!(
            "DRM dumb buffer pitch {pitch} is smaller than row byte width {row_bytes}"
        ));
    }
    let needed_src = width
        .checked_mul(height)
        .ok_or_else(|| "framebuffer dimensions overflow".to_string())?;
    if src.len() < needed_src {
        return Err(format!(
            "source pixel buffer len {} < required {}",
            src.len(),
            needed_src
        ));
    }
    let needed_dst = pitch
        .checked_mul(height)
        .ok_or_else(|| "DRM dumb buffer pitch * height overflow".to_string())?;
    if dst.len() < needed_dst {
        return Err(format!(
            "DRM dumb buffer mapping len {} < required {}",
            dst.len(),
            needed_dst
        ));
    }

    for row in 0..height {
        let src_row = &src[row * width..(row + 1) * width];
        let dst_row = &mut dst[row * pitch..row * pitch + row_bytes];
        for (col, px) in src_row.iter().enumerate() {
            let offset = col * 4;
            dst_row[offset..offset + 4].copy_from_slice(&px.0.to_ne_bytes());
        }
    }
    Ok(())
}

/// Draw a crisp software pointer arrow (dark outline + bright fill) onto `fb`.
pub fn draw_software_cursor(
    fb: &mut [Xrgb8888Pixel],
    width: usize,
    height: usize,
    px: i32,
    py: i32,
) {
    // 14-row pointer bitmap: '.' transparent, '#' outline, 'W' white fill.
    const CURSOR: [&str; 14] = [
        "#.........",
        "##........",
        "#W#.......",
        "#WW#......",
        "#WWW#.....",
        "#WWWW#....",
        "#WWWWW#...",
        "#WWWWWW#..",
        "#WWWWWWW#.",
        "#WWWWW####",
        "#WW#WW#...",
        "#W#.#WW#..",
        "##...#W#..",
        "#.....##..",
    ];
    let outline = Xrgb8888Pixel::from_rgb(15, 23, 42);
    let fill = Xrgb8888Pixel::from_rgb(248, 250, 252);

    for (dy, row) in CURSOR.iter().enumerate() {
        let y = py + dy as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for (dx, ch) in row.bytes().enumerate() {
            let x = px + dx as i32;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let idx = (y as usize) * width + (x as usize);
            if idx >= fb.len() {
                continue;
            }
            match ch {
                b'#' => fb[idx] = outline,
                b'W' => fb[idx] = fill,
                _ => {}
            }
        }
    }
}

/// Return `true` if `evdev_code` is Left Super (`KEY_LEFTMETA=125`) or Right Super (`KEY_RIGHTMETA=126`).
#[inline]
pub fn is_super_keycode(evdev_code: u32) -> bool {
    evdev_code == 125 || evdev_code == 126
}

/// Map `Ctrl+Alt+F1..F12` Linux evdev keycodes to a VT number `1..=12`.
pub fn vt_switch_target(evdev_code: u32, ctrl: bool, alt: bool) -> Option<i32> {
    if !ctrl || !alt {
        return None;
    }
    match evdev_code {
        59..=68 => Some((evdev_code - 58) as i32), // KEY_F1..=KEY_F10 -> VT 1..=10
        87 => Some(11),                            // KEY_F11
        88 => Some(12),                            // KEY_F12
        _ => None,
    }
}

/// Translate common Linux evdev keycodes (`<linux/input-event-codes.h>`) to Slint key strings.
pub fn linux_keycode_to_slint_key(evdev_code: u32, shift: bool) -> Option<SharedString> {
    let special = match evdev_code {
        1 => Some(Key::Escape),
        14 => Some(Key::Backspace),
        15 => Some(Key::Tab),
        28 | 96 => Some(Key::Return),
        57 => Some(Key::Space),
        42 => Some(Key::Shift),
        54 => Some(Key::ShiftR),
        29 => Some(Key::Control),
        97 => Some(Key::ControlR),
        56 | 100 => Some(Key::Alt),
        125 => Some(Key::Meta),
        126 => Some(Key::MetaR),
        102 => Some(Key::Home),
        103 => Some(Key::UpArrow),
        104 => Some(Key::PageUp),
        105 => Some(Key::LeftArrow),
        106 => Some(Key::RightArrow),
        107 => Some(Key::End),
        108 => Some(Key::DownArrow),
        109 => Some(Key::PageDown),
        110 => Some(Key::Insert),
        111 => Some(Key::Delete),
        _ => None,
    };
    if let Some(k) = special {
        return Some(k.into());
    }

    let ch = match evdev_code {
        2 => {
            if shift {
                '!'
            } else {
                '1'
            }
        }
        3 => {
            if shift {
                '@'
            } else {
                '2'
            }
        }
        4 => {
            if shift {
                '#'
            } else {
                '3'
            }
        }
        5 => {
            if shift {
                '$'
            } else {
                '4'
            }
        }
        6 => {
            if shift {
                '%'
            } else {
                '5'
            }
        }
        7 => {
            if shift {
                '^'
            } else {
                '6'
            }
        }
        8 => {
            if shift {
                '&'
            } else {
                '7'
            }
        }
        9 => {
            if shift {
                '*'
            } else {
                '8'
            }
        }
        10 => {
            if shift {
                '('
            } else {
                '9'
            }
        }
        11 => {
            if shift {
                ')'
            } else {
                '0'
            }
        }
        12 => {
            if shift {
                '_'
            } else {
                '-'
            }
        }
        13 => {
            if shift {
                '+'
            } else {
                '='
            }
        }
        16 => {
            if shift {
                'Q'
            } else {
                'q'
            }
        }
        17 => {
            if shift {
                'W'
            } else {
                'w'
            }
        }
        18 => {
            if shift {
                'E'
            } else {
                'e'
            }
        }
        19 => {
            if shift {
                'R'
            } else {
                'r'
            }
        }
        20 => {
            if shift {
                'T'
            } else {
                't'
            }
        }
        21 => {
            if shift {
                'Y'
            } else {
                'y'
            }
        }
        22 => {
            if shift {
                'U'
            } else {
                'u'
            }
        }
        23 => {
            if shift {
                'I'
            } else {
                'i'
            }
        }
        24 => {
            if shift {
                'O'
            } else {
                'o'
            }
        }
        25 => {
            if shift {
                'P'
            } else {
                'p'
            }
        }
        26 => {
            if shift {
                '{'
            } else {
                '['
            }
        }
        27 => {
            if shift {
                '}'
            } else {
                ']'
            }
        }
        30 => {
            if shift {
                'A'
            } else {
                'a'
            }
        }
        31 => {
            if shift {
                'S'
            } else {
                's'
            }
        }
        32 => {
            if shift {
                'D'
            } else {
                'd'
            }
        }
        33 => {
            if shift {
                'F'
            } else {
                'f'
            }
        }
        34 => {
            if shift {
                'G'
            } else {
                'g'
            }
        }
        35 => {
            if shift {
                'H'
            } else {
                'h'
            }
        }
        36 => {
            if shift {
                'J'
            } else {
                'j'
            }
        }
        37 => {
            if shift {
                'K'
            } else {
                'k'
            }
        }
        38 => {
            if shift {
                'L'
            } else {
                'l'
            }
        }
        39 => {
            if shift {
                ':'
            } else {
                ';'
            }
        }
        40 => {
            if shift {
                '"'
            } else {
                '\''
            }
        }
        41 => {
            if shift {
                '~'
            } else {
                '`'
            }
        }
        43 => {
            if shift {
                '|'
            } else {
                '\\'
            }
        }
        44 => {
            if shift {
                'Z'
            } else {
                'z'
            }
        }
        45 => {
            if shift {
                'X'
            } else {
                'x'
            }
        }
        46 => {
            if shift {
                'C'
            } else {
                'c'
            }
        }
        47 => {
            if shift {
                'V'
            } else {
                'v'
            }
        }
        48 => {
            if shift {
                'B'
            } else {
                'b'
            }
        }
        49 => {
            if shift {
                'N'
            } else {
                'n'
            }
        }
        50 => {
            if shift {
                'M'
            } else {
                'm'
            }
        }
        51 => {
            if shift {
                '<'
            } else {
                ','
            }
        }
        52 => {
            if shift {
                '>'
            } else {
                '.'
            }
        }
        53 => {
            if shift {
                '?'
            } else {
                '/'
            }
        }
        _ => return None,
    };
    Some(ch.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb8888_pixel_rgb_and_alpha_blend() {
        let mut px = Xrgb8888Pixel::from_rgb(100, 150, 200);
        assert_eq!(px.0, 0xFF64_96C8);

        // Opaque overwrite
        px.blend(PremultipliedRgbaColor {
            alpha: 255,
            red: 10,
            green: 20,
            blue: 30,
        });
        assert_eq!(px.0, 0xFF0A_141E);

        // Transparent no-op
        px.blend(PremultipliedRgbaColor {
            alpha: 0,
            red: 99,
            green: 99,
            blue: 99,
        });
        assert_eq!(px.0, 0xFF0A_141E);

        // Half-alpha premultiplied blend over black
        let mut black = Xrgb8888Pixel::from_rgb(0, 0, 0);
        black.blend(PremultipliedRgbaColor {
            alpha: 128,
            red: 128,
            green: 64,
            blue: 32,
        });
        assert_eq!(black.0, 0xFF80_4020);
    }

    #[test]
    fn dumb_buffer_copy_respects_padded_pitch_and_rejects_short_buffers() {
        let src = vec![
            Xrgb8888Pixel::from_rgb(1, 2, 3),
            Xrgb8888Pixel::from_rgb(4, 5, 6),
            Xrgb8888Pixel::from_rgb(7, 8, 9),
            Xrgb8888Pixel::from_rgb(10, 11, 12),
        ];
        // width=2 (8 bytes) with 12-byte pitch (4 bytes padding per row)
        let mut dst = vec![0xAAu8; 24];
        copy_xrgb8888_to_dumb_buffer(&src, 2, 2, &mut dst, 12).unwrap();

        assert_eq!(&dst[0..4], &src[0].0.to_ne_bytes());
        assert_eq!(&dst[4..8], &src[1].0.to_ne_bytes());
        assert_eq!(&dst[8..12], &[0xAA; 4]);
        assert_eq!(&dst[12..16], &src[2].0.to_ne_bytes());
        assert_eq!(&dst[16..20], &src[3].0.to_ne_bytes());
        assert_eq!(&dst[20..24], &[0xAA; 4]);

        assert!(copy_xrgb8888_to_dumb_buffer(&src, 2, 2, &mut dst, 4).is_err());
        assert!(copy_xrgb8888_to_dumb_buffer(&src[..3], 2, 2, &mut dst, 12).is_err());
        assert!(copy_xrgb8888_to_dumb_buffer(&src, 2, 2, &mut dst[..20], 12).is_err());
    }

    #[test]
    fn cursor_overlay_and_keycode_mapping_handle_bounds_and_modifiers() {
        let mut fb = vec![Xrgb8888Pixel::background(); 16 * 16];
        draw_software_cursor(&mut fb, 16, 16, 12, 10);
        // Hotspot (12, 10) is cursor outline; (13, 12) is white fill.
        assert_eq!(fb[10 * 16 + 12], Xrgb8888Pixel::from_rgb(15, 23, 42));
        assert_eq!(fb[12 * 16 + 13], Xrgb8888Pixel::from_rgb(248, 250, 252));

        // Edge clipping must not panic
        draw_software_cursor(&mut fb, 16, 16, -4, -4);
        draw_software_cursor(&mut fb, 16, 16, 15, 15);

        assert!(is_super_keycode(125));
        assert!(is_super_keycode(126));
        assert!(!is_super_keycode(1));

        assert_eq!(
            linux_keycode_to_slint_key(125, false),
            Some(SharedString::from(Key::Meta))
        );
        assert_eq!(
            linux_keycode_to_slint_key(1, false),
            Some(SharedString::from(Key::Escape))
        );
        assert_eq!(
            linux_keycode_to_slint_key(30, false),
            Some(SharedString::from("a"))
        );
        assert_eq!(
            linux_keycode_to_slint_key(30, true),
            Some(SharedString::from("A"))
        );

        assert_eq!(vt_switch_target(60, true, true), Some(2));
        assert_eq!(vt_switch_target(60, true, false), None);
        assert_eq!(vt_switch_target(88, true, true), Some(12));
    }
}
