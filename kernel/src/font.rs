//! The eliOS console font. The glyphs are drawn in `font/eli-8x16.txt` and
//! compiled into tables by `build.rs`.

pub const WIDTH: usize = 8;
pub const HEIGHT: usize = 16;

include!(concat!(env!("OUT_DIR"), "/font.rs"));

/// The glyph for `c`: one byte per row, leftmost pixel in the top bit.
pub fn glyph(c: char) -> &'static [u8; HEIGHT] {
    if c.is_ascii() {
        return &ASCII[c as usize];
    }
    match EXTRA.binary_search_by_key(&c, |&(ch, _)| ch) {
        Ok(index) => &EXTRA[index].1,
        Err(_) => &FALLBACK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn every_printable_ascii_char_has_its_own_glyph() {
            for c in ' '..='~' {
                assert_ne!(glyph(c), &FALLBACK, "{c:?} has no glyph");
            }
        }

        fn space_is_blank() {
            assert!(glyph(' ').iter().all(|&row| row == 0));
        }

        fn unknown_chars_get_the_fallback() {
            assert_eq!(glyph('\u{7}'), &FALLBACK);
            assert_eq!(glyph('é'), &FALLBACK);
            assert_eq!(glyph('\u{1F600}'), &FALLBACK);
        }

        fn glyphs_keep_the_spacing_column_clear() {
            for c in (' '..='~').filter(|&c| c != '_') {
                assert!(glyph(c).iter().all(|&row| row & 1 == 0), "{c:?} touches column 7");
            }
        }
    }
}
