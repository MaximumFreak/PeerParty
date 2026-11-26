//! Finding a function in a module by its bytes: a pattern of hex bytes with `?` / `??`
//! wildcards ("40 53 48 83 EC 20 ?? 8B D9"), as the player writes them in the hooks file
//! (config/hooks.rs). Only a module's executable sections are searched, and a pattern
//! counts only when it matches exactly one place: one that matches several would hook
//! whichever came first, maybe not the function meant.

use crate::sys::pe;

/// The most bytes a pattern may have, and the fewest it must fix (not wildcards): a
/// pattern of a few bytes matches by chance.
pub const MAX_LEN: usize = 256;
pub const MIN_FIXED: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// Each byte, None for a wildcard.
    bytes: Vec<Option<u8>>,
    /// Where the first fixed byte is: the search looks for it, then compares the rest.
    anchor: usize,
}

/// Where a pattern matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Nowhere,
    At(usize),
    /// In this many places (counting stops at a few).
    Many(usize),
}

impl Pattern {
    pub fn parse(text: &str) -> Result<Pattern, String> {
        let mut bytes = Vec::new();
        for token in text.split_whitespace() {
            if bytes.len() == MAX_LEN {
                return Err(format!("longer than {MAX_LEN} bytes"));
            }
            if token.chars().all(|c| c == '?') && token.len() <= 2 {
                bytes.push(None);
                continue;
            }
            match u8::from_str_radix(token, 16) {
                Ok(b) if token.len() == 2 => bytes.push(Some(b)),
                _ => return Err(format!("{token:?} is not a hex byte or ??")),
            }
        }
        let fixed = bytes.iter().filter(|b| b.is_some()).count();
        if fixed < MIN_FIXED {
            return Err(format!("only {fixed} fixed bytes, at least {MIN_FIXED} needed"));
        }
        let anchor = bytes.iter().position(Option::is_some).unwrap_or(0);
        Ok(Pattern { bytes, anchor })
    }

    /// Where it matches in `hay` (offsets), at most `max` of them.
    pub fn matches(&self, hay: &[u8], max: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let Some(first) = self.bytes[self.anchor] else { return out };
        if hay.len() < self.bytes.len() {
            return out;
        }
        let last_start = hay.len() - self.bytes.len();
        let mut from = 0;
        while from <= last_start && out.len() < max {
            // The anchor byte's next place, then the whole pattern there.
            let window = &hay[from + self.anchor..=last_start + self.anchor];
            let Some(i) = window.iter().position(|&b| b == first) else { break };
            let start = from + i;
            if self.bytes.iter().zip(&hay[start..]).all(|(p, h)| p.is_none_or(|p| p == *h)) {
                out.push(start);
            }
            from = start + 1;
        }
        out
    }
}

/// Where `pattern` is in the executable sections of the module loaded at `base`. A
/// section the process can't read (a packer's, still locked) is skipped.
pub fn find(base: usize, pattern: &Pattern) -> Found {
    let Ok(ranges) = (unsafe { pe::code_ranges(base as *const u8) }) else { return Found::Nowhere };
    let mut found = Vec::new();
    for (at, len) in ranges {
        if !crate::sys::read::readable(at, len) {
            continue;
        }
        // SAFETY: the whole range is mapped and readable, checked just above; it's the
        // module's own image, which stays loaded while its hooks are made.
        let hay = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
        found.extend(pattern.matches(hay, 3 - found.len()).into_iter().map(|o| at + o));
        if found.len() >= 2 {
            break;
        }
    }
    match found.as_slice() {
        [] => Found::Nowhere,
        [one] => Found::At(*one),
        many => Found::Many(many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_parse() {
        let p = Pattern::parse("40 53 48 83 ec 20 ?? 8B D9 ? 85").unwrap();
        assert_eq!(p.bytes.len(), 11);
        assert_eq!(p.bytes[6], None);
        assert_eq!(p.bytes[9], None);
        assert_eq!(p.bytes[4], Some(0xEC));
        let p = Pattern::parse("?? ?? 01 02 03 04 05 06").unwrap();
        assert_eq!(p.anchor, 2);
        assert!(Pattern::parse("").is_err());
        assert!(Pattern::parse("01 02 03 04 05").is_err(), "too few fixed bytes");
        assert!(Pattern::parse("01 02 03 ?? ?? ?? 04 05").is_err(), "wildcards don't count");
        assert!(Pattern::parse("01 02 03 04 05 0G").is_err());
        assert!(Pattern::parse("01 02 03 04 05 123").is_err());
        assert!(Pattern::parse("01 02 03 04 05 06 ???").is_err());
        assert!(Pattern::parse(&"AA ".repeat(MAX_LEN)).is_ok());
        assert!(Pattern::parse(&"AA ".repeat(MAX_LEN + 1)).is_err());
    }

    #[test]
    fn patterns_match_with_wildcards() {
        let p = Pattern::parse("?? 10 11 12 ?? 14 15 16").unwrap();
        let mut hay = vec![0u8; 64];
        hay[20..28].copy_from_slice(&[0xFF, 0x10, 0x11, 0x12, 0x77, 0x14, 0x15, 0x16]);
        assert_eq!(p.matches(&hay, 3), vec![20]);
        hay[50..58].copy_from_slice(&[0x00, 0x10, 0x11, 0x12, 0x00, 0x14, 0x15, 0x16]);
        assert_eq!(p.matches(&hay, 3), vec![20, 50]);
        assert_eq!(p.matches(&hay, 1), vec![20]);
        // At the very end, and not past it.
        let end = [0u8, 0x10, 0x11, 0x12, 0, 0x14, 0x15, 0x16];
        assert_eq!(p.matches(&end, 3), vec![0]);
        assert_eq!(p.matches(&end[..7], 3), Vec::<usize>::new());
        // Near misses.
        assert_eq!(p.matches(&[0, 0x10, 0x11, 0x12, 0, 0x14, 0x15, 0x17], 3), Vec::<usize>::new());
    }

    /// A function of this test's own image, found by its first bytes and only there.
    #[test]
    fn finds_a_function_in_a_loaded_module() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        let ranges = unsafe { pe::code_ranges(base as *const u8).unwrap() };
        let (at, len) = ranges[0];
        let code = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
        // Some 32-byte run in the code that occurs once: its pattern finds it.
        let unique = (0..len.saturating_sub(32))
            .step_by(997)
            .find_map(|o| {
                let text: Vec<String> = code[o..o + 32].iter().map(|b| format!("{b:02X}")).collect();
                let p = Pattern::parse(&text.join(" ")).ok()?;
                (find(base, &p) == Found::At(at + o)).then_some(o)
            });
        assert!(unique.is_some(), "no unique 32-byte run found");
        // A run of one repeated byte isn't unique.
        let p = Pattern::parse(&"CC ".repeat(8)).unwrap();
        assert!(matches!(find(base, &p), Found::Many(_) | Found::Nowhere));
        assert_eq!(find(0, &p), Found::Nowhere);
    }
}
