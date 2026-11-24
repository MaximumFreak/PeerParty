//! The few regular expressions `mainexe` matches file and folder names against, without
//! a regex crate (it was a megabyte and a half of the exe for these four patterns). The
//! rules stay written as the Game Bar importer writes them; this reads the part of the
//! syntax they use: literals, `.`, `\d`, `\.`-style escapes, `[...]` sets, `(a|b)`
//! groups (`(?P<name>...)` too), `?` and `*`, `^` and `$`. ASCII letters match either
//! case, like `(?i)`.

/// One parsed pattern.
#[derive(Debug)]
pub struct Pattern(Node);

#[derive(Debug)]
enum Node {
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    /// The node, at most once (`?`) or any number of times (`*`), as many as fit first.
    Repeat(Box<Node>, bool),
    Byte(u8),
    Any,
    Digit,
    Set(Vec<(u8, u8)>),
    Start,
    End,
}

impl Pattern {
    /// `src` parsed; a leading `(?i)` is allowed (case is always ignored). Panics on
    /// syntax outside the subset above: the patterns are constants in this program.
    pub fn new(src: &str) -> Pattern {
        let src = src.strip_prefix("(?i)").unwrap_or(src).as_bytes();
        let mut at = 0;
        let node = alt(src, &mut at);
        assert!(at == src.len(), "pattern {:?}: unexpected ')' at {at}", String::from_utf8_lossy(src));
        Pattern(node)
    }

    /// Where a match starting at `from` ends, if one does (the first the pattern's
    /// order finds, like a backtracking regex).
    pub fn match_at(&self, s: &str, from: usize) -> Option<usize> {
        let mut end = None;
        go(&self.0, s.as_bytes(), from, &mut |e| {
            end = Some(e);
            true
        });
        end
    }

    /// Whether it matches anywhere in `s`.
    pub fn is_match(&self, s: &str) -> bool {
        self.find(s, 0).is_some()
    }

    /// The leftmost match at or after `from`: (start, end).
    pub fn find(&self, s: &str, from: usize) -> Option<(usize, usize)> {
        (from..=s.len()).find_map(|i| self.match_at(s, i).map(|e| (i, e)))
    }
}

fn alt(p: &[u8], at: &mut usize) -> Node {
    let mut options = vec![seq(p, at)];
    while p.get(*at) == Some(&b'|') {
        *at += 1;
        options.push(seq(p, at));
    }
    if options.len() == 1 { options.pop().unwrap() } else { Node::Alt(options) }
}

fn seq(p: &[u8], at: &mut usize) -> Node {
    let mut items = Vec::new();
    while let Some(&c) = p.get(*at) {
        let atom = match c {
            b'|' | b')' => break,
            b'(' => {
                *at += 1;
                // A named group is matched like any other.
                if p[*at..].starts_with(b"?P<") {
                    *at += p[*at..].iter().position(|&b| b == b'>').expect("named group") + 1;
                }
                let inner = alt(p, at);
                assert_eq!(p.get(*at), Some(&b')'), "unclosed group");
                *at += 1;
                inner
            }
            b'[' => {
                *at += 1;
                let mut ranges = Vec::new();
                while p[*at] != b']' {
                    let lo = p[*at];
                    if p[*at + 1] == b'-' && p[*at + 2] != b']' {
                        ranges.push((lo, p[*at + 2]));
                        *at += 3;
                    } else {
                        ranges.push((lo, lo));
                        *at += 1;
                    }
                }
                *at += 1;
                Node::Set(ranges)
            }
            b'\\' => {
                *at += 2;
                match p[*at - 1] {
                    b'd' => Node::Digit,
                    b => Node::Byte(b),
                }
            }
            b'.' => {
                *at += 1;
                Node::Any
            }
            b'^' => {
                *at += 1;
                Node::Start
            }
            b'$' => {
                *at += 1;
                Node::End
            }
            b => {
                assert!(!matches!(b, b'*' | b'?' | b'+' | b'{'), "nothing to repeat");
                *at += 1;
                Node::Byte(b)
            }
        };
        items.push(match p.get(*at) {
            Some(b'?') => {
                *at += 1;
                Node::Repeat(Box::new(atom), false)
            }
            Some(b'*') => {
                *at += 1;
                Node::Repeat(Box::new(atom), true)
            }
            Some(b'+' | b'{') => panic!("unsupported repeat"),
            _ => atom,
        });
    }
    Node::Seq(items)
}

/// Matches `n` at `i`, calling `k` with each place it could end (most first) until `k`
/// says that one will do.
fn go(n: &Node, s: &[u8], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    let one = |ok: bool| ok.then_some(i + 1);
    let next = match n {
        Node::Seq(items) => return seq_go(items, s, i, k),
        Node::Alt(options) => return options.iter().any(|o| go(o, s, i, k)),
        Node::Repeat(inner, many) => return repeat(inner, *many, s, i, k),
        Node::Start => return i == 0 && k(i),
        Node::End => return i == s.len() && k(i),
        _ if i >= s.len() => None,
        Node::Byte(b) => one(s[i].eq_ignore_ascii_case(b)),
        Node::Any => one(s[i] != b'\n'),
        Node::Digit => one(s[i].is_ascii_digit()),
        Node::Set(ranges) => {
            let c = s[i];
            one(ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c) || (lo..=hi).contains(&c.to_ascii_lowercase()) || (lo..=hi).contains(&c.to_ascii_uppercase())))
        }
    };
    next.is_some_and(|e| k(e))
}

fn seq_go(items: &[Node], s: &[u8], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    match items.split_first() {
        None => k(i),
        Some((first, rest)) => go(first, s, i, &mut |e| seq_go(rest, s, e, k)),
    }
}

fn repeat(inner: &Node, many: bool, s: &[u8], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
    // Once more first (greedy), never on an empty step (it would loop).
    let more = go(inner, s, i, &mut |e| e > i && if many { repeat(inner, true, s, e, k) } else { k(e) });
    more || k(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern shape the rules use, against the regex crate's answers.
    #[test]
    fn matches_like_regex() {
        let patterns = [
            r"(?i)^(unins\d*|uninstall.*|.*crash[ _-]?(handler|report|pad|uploader|sender|dump|rpt).*|vc_?redist.*|ue[345]?prereqsetup.*|easyanticheat_(eos_)?setup|(.*[ _-])?(setup|installer)([ _-].*)?|(.*[ _-])?(updater|patcher)(64)?|.*language[ ._-]?(changer|select(or)?|setup).*|battle\.net-setup.*|mod ?manager.*)\.exe$",
            r"(?i)^(_?commonredist|redistributables?|\.net|prereqs?|installers?)$",
            r"(?i)[ _.-]?((dx|d3d)(9|10|11|12)|vulkan|vk)(?P<sep>$|[ _.-])",
            r"(?i)[ _.-]?((dx|d3d)(9|10|11))(?P<sep>$|[ _.-])",
        ];
        let names = [
            "unins000.exe", "Uninstall Game.exe", "UnityCrashHandler64.exe", "crashpad_handler.exe", "CrashReport.exe",
            "vc_redist.x64.exe", "VCREDIST.exe", "UE4PrereqSetup_x64.exe", "EasyAntiCheat_EOS_Setup.exe",
            "EasyAntiCheat_Setup.exe", "setup.exe", "game setup.exe", "Game_Installer_v2.exe", "updater64.exe",
            "Game-Patcher.exe", "language_selector.exe", "LanguageChanger.exe", "Battle.net-Setup-enUS.exe",
            "Mod Manager.exe", "modmanager.exe", "Game.exe", "setupgame.exe", "game.exe.bak", "unins.exe", "uninsX.exe",
            "_CommonRedist", "commonredist", "Redistributable", "redistributables", ".NET", "xnet", "prereq", "Installers",
            "installer", "game_dx11", "Game-D3D12", "game_dx11_vk", "gamevk", "game.vulkan.x", "dx9", "game dx10 ",
            "game_dx13", "game_dx", "dx11game", "DX12", "", "a", "-", "game__dx11", "über_dx11", "日本_dx9",
        ];
        for p in patterns {
            let ours = Pattern::new(p);
            let re = regex::Regex::new(p).unwrap();
            for n in names {
                assert_eq!(ours.is_match(n), re.is_match(n), "{p} on {n:?}");
                let theirs = re.find(n).map(|m| (m.start(), m.end()));
                assert_eq!(ours.find(n, 0), theirs, "{p} on {n:?}");
            }
        }
    }
}
