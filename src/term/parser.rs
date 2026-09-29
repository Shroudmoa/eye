//! An incremental VT parser.
//!
//! The grammar handled here is the part of ECMA-48 / DEC that real programs
//! actually emit: C0 controls, CSI, OSC, DCS and the ESC forms. The parser is
//! fed arbitrary byte chunks — it is a byte-oriented state machine, so it never
//! has to see a whole escape sequence in one `write`, which matters because a
//! PTY will happily split one in half.

/// A decoded control sequence.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// A printable character, already decoded from UTF-8.
    Print(char),
    /// Carriage return.
    Cr,
    /// Line feed.
    Lf,
    /// Reverse index (`ESC M`).
    ReverseIndex,
    /// Next line (`ESC E`): CR + LF.
    NextLine,
    /// Move to the next tab stop, `n` times.
    Tab(u16),
    /// Move backwards `n` tab stops.
    BackTab(u16),
    /// Backspace.
    Backspace,
    /// Bell: no screen effect.
    Bell,
    /// Vertical tab / form feed, treated as a line feed.
    VTab,
    /// A two-byte ESC form: save/restore cursor, index, tab set, RIS, keypad.
    Esc(u8),
    /// `ESC ( B` and relatives: designate a character set. Recorded, not
    /// applied — the rendered grid stays UTF-8.
    CharsetDesignate,

    /// CSI with its parsed parameters.
    Csi(Params),
    /// OSC with its identifier and payload.
    Osc { code: u8, data: String },
    /// DCS / APC / PM / SOS: consumed and discarded. `eye` replays the raw
    /// byte stream for fidelity, so these are never rendered here.
    Ignored,

    /// A device status report: the CSI's parameter groups, undecoded.
    DeviceReport(Vec<Vec<u16>>),
}

/// Parsed CSI parameters, keeping the "was this parameter supplied" distinction
/// that defaults like `CSI ;5H` depend on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params {
    /// `<=>?` prefix byte, if present.
    pub private: Option<u8>,
    pub intermediates: Vec<u8>,
    /// Colon-separated sub-parameters are nested, so `CSI 4:3H` is one group of
    /// two sub-parameters rather than two groups.
    pub groups: Vec<Vec<u16>>,
    pub final_byte: u8,
}

impl Params {
    /// Parameter `i`, treating 0 and "absent" as `default`.
    pub fn get(&self, i: usize, default: u16) -> u16 {
        match self.groups.get(i) {
            Some(g) => g.first().copied().filter(|v| *v != 0).unwrap_or(default),
            None => default,
        }
    }

    /// Parameter `i`, where an explicit 0 really is zero.
    pub fn get_raw(&self, i: usize, default: u16) -> u16 {
        self.groups.get(i).and_then(|g| g.first().copied()).unwrap_or(default)
    }

    /// Does parameter `i` exist? A defaulted parameter (`CSI m`) exists but
    /// carries no value, which is different from an absent one (`CSI 1m`).
    pub fn is_set(&self, i: usize) -> bool {
        self.groups.get(i).is_some()
    }

    /// Sub-parameter `j` of group `i`.
    pub fn sub(&self, i: usize, j: usize) -> Option<u16> {
        self.groups.get(i).and_then(|g| g.get(j)).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// Just saw `ESC (`: the next byte designates a character set.
    EscapeDesignate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    OscEscape,
    /// DCS / APC / PM / SOS payload: skip until ST.
    StringIgnore,
    StringIgnoreEscape,
}

/// Longest OSC payload we retain. Titles and OSC 133 marks are short; anything
/// larger is a paste or a binary blob and is truncated rather than buffered
/// without bound.
const MAX_OSC: usize = 8 * 1024;

/// Cursor over the current parameter group, used to fold digits together.
struct GroupBuilder<'a> {
    group: &'a mut Vec<u16>,
}

impl GroupBuilder<'_> {
    fn push_or_extend(&mut self, digit: u16) {
        match self.group.len() {
            0 => self.group.push(digit),
            1 => self.group[0] = self.group[0].saturating_mul(10).saturating_add(digit),
            // Only reachable after a `:`; digits then fill the open sub-parameter.
            n => {
                let last = n - 1;
                self.group[last] = self.group[last].saturating_mul(10).saturating_add(digit);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.group.is_empty()
    }

    fn push(&mut self, v: u16) {
        self.group.push(v);
    }
}

/// The parser state machine.
pub struct Parser {
    state: State,
    /// Partially received UTF-8 scalar.
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
    params: Params,
    /// Index of the parameter group currently being filled.
    group_index: usize,
    /// Digits of the OSC identifier, still being read.
    osc_digits: Vec<u8>,
    osc_code: Option<u8>,
    osc_buf: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            state: State::Ground,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
            params: Params::default(),
            group_index: 0,
            osc_digits: Vec::new(),
            osc_code: None,
            osc_buf: Vec::new(),
        }
    }

    /// Reset to ground, discarding any partial sequence. Used after RIS.
    pub fn reset(&mut self) {
        *self = Parser::new();
    }

    /// Finish a stream, emitting a replacement for any half-received scalar.
    ///
    /// Without this a character cut in half by a `read` boundary would simply
    /// disappear instead of becoming U+FFFD.
    pub fn flush(&mut self) -> Vec<Action> {
        if self.utf8_need > 0 {
            self.utf8_need = 0;
            self.utf8_len = 0;
            return vec![Action::Print('\u{fffd}')];
        }
        Vec::new()
    }

    /// Feed bytes, producing the actions they encode.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut out = Vec::with_capacity(bytes.len().min(4096));
        for &b in bytes {
            self.step(b, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, out: &mut Vec<Action>) {
        // UTF-8 continuation bytes never interrupt a sequence.
        if self.utf8_need > 0 {
            if b & 0xc0 == 0x80 {
                self.utf8[self.utf8_len] = b;
                self.utf8_len += 1;
                if self.utf8_len == self.utf8_need {
                    self.utf8_need = 0;
                    match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
                        Ok(s) => out.push(Action::Print(s.chars().next().unwrap_or('\u{fffd}'))),
                        Err(_) => out.push(Action::Print('\u{fffd}')),
                    }
                    self.utf8_len = 0;
                }
                return;
            }
            // Truncated sequence: emit a replacement, then reprocess this byte.
            self.utf8_need = 0;
            self.utf8_len = 0;
            out.push(Action::Print('\u{fffd}'));
        }

        match self.state {
            State::Ground => self.ground(b, out),
            State::Escape => self.escape(b, out),
            State::EscapeDesignate => {
                self.state = State::Ground;
                out.push(Action::CharsetDesignate);
            }
            State::CsiEntry | State::CsiParam | State::CsiIntermediate | State::CsiIgnore => {
                self.csi(b, out)
            }
            State::OscString | State::OscEscape => self.osc(b, out),
            State::StringIgnore | State::StringIgnoreEscape => self.string(b, out),
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x1b => {
                self.state = State::Escape;
                self.params = Params::default();
            }
            0x07 => out.push(Action::Bell),
            0x08 => out.push(Action::Backspace),
            0x09 => out.push(Action::Tab(1)),
            0x0a | 0x0c => out.push(Action::Lf),
            0x0b => out.push(Action::VTab),
            0x0d => out.push(Action::Cr),
            0x0e | 0x0f => {} // shift in/out: the rendered grid stays UTF-8
            0x00..=0x06 | 0x10..=0x17 | 0x19 | 0x1c..=0x1f => {}
            _ => {
                if b < 0x80 {
                    out.push(Action::Print(b as char));
                } else {
                    self.start_utf8(b, out);
                }
            }
        }
    }

    /// Begin decoding a multi-byte UTF-8 scalar.
    fn start_utf8(&mut self, b: u8, out: &mut Vec<Action>) {
        self.utf8_need = match b {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            // Continuation byte or overlong lead: not a valid start.
            _ => {
                out.push(Action::Print('\u{fffd}'));
                return;
            }
        };
        self.utf8[0] = b;
        self.utf8_len = 1;
    }

    fn escape(&mut self, b: u8, out: &mut Vec<Action>) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.state = State::CsiEntry;
                self.params = Params::default();
                self.group_index = 0;
            }
            b']' => {
                self.state = State::OscString;
                self.osc_code = None;
                self.osc_digits.clear();
                self.osc_buf.clear();
            }
            b'P' | b'X' | b'^' | b'_' => self.state = State::StringIgnore,
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => self.state = State::EscapeDesignate,
            _ => out.push(Action::Esc(b)),
        }
    }

    fn csi(&mut self, b: u8, out: &mut Vec<Action>) {
        match b {
            0x30..=0x39 => {
                self.state = State::CsiParam;
                let digit = (b - b'0') as u16;
                self.current_group_mut().push_or_extend(digit);
            }
            b';' => {
                // Close this group and move on; the next token opens a new one,
                // which is what makes `CSI ;5H` two parameters, not one.
                self.state = State::CsiParam;
                self.group_index += 1;
                self.ensure_groups();
            }
            b':' => {
                // A colon opens another sub-parameter inside the same group.
                self.state = State::CsiParam;
                let mut group = self.current_group_mut();
                if group.is_empty() {
                    group.push(0);
                }
                group.push(0);
            }
            0x3c..=0x3f => {
                self.params.private = Some(b);
                self.state = State::CsiParam;
            }
            0x20..=0x2f => {
                self.params.intermediates.push(b);
                self.state = State::CsiIntermediate;
            }
            0x40..=0x7e => {
                self.state = State::Ground;
                self.params.final_byte = b;
                self.ensure_groups();
                let params = std::mem::take(&mut self.params);
                self.group_index = 0;
                match b {
                    // Device status reports and screen dumps owe a reply; the
                    // screen layer decides what to send back.
                    b'n' | b'c' | b'S' => {
                        out.push(Action::DeviceReport(params.groups.clone()))
                    }
                    _ => out.push(Action::Csi(params)),
                }
            }
            0x1b => {
                self.state = State::Escape;
                self.params = Params::default();
                self.group_index = 0;
            }
            0x00..=0x1f => self.state = State::CsiIgnore,
            _ => self.state = State::CsiIgnore,
        }
    }

    /// Materialise groups up to and including the one being filled.
    fn ensure_groups(&mut self) {
        while self.params.groups.len() <= self.group_index {
            self.params.groups.push(Vec::new());
        }
    }

    /// The group digits currently extend. Groups hold at most two values: the
    /// value itself, plus a second slot only used while a colon-separated
    /// sub-parameter is open.
    fn current_group_mut(&mut self) -> GroupBuilder<'_> {
        self.ensure_groups();
        GroupBuilder {
            group: self.params.groups.last_mut().expect("ensure_groups ran"),
        }
    }

    fn osc(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.state == State::OscEscape {
            self.state = State::OscString;
            if b == b'\\' {
                self.finish_osc(out);
            } else if b == 0x07 {
                self.finish_osc(out);
            } else if self.osc_buf.len() < MAX_OSC {
                self.osc_buf.push(b);
            }
            return;
        }
        match b {
            0x07 => self.finish_osc(out),
            0x1b => self.state = State::OscEscape,
            _ if self.osc_code.is_none() => self.osc_identifier(b),
            _ => {
                if self.osc_buf.len() < MAX_OSC {
                    self.osc_buf.push(b);
                }
            }
        }
    }

    /// Read the OSC identifier: digits, terminated by `;`. Anything else means
    /// there was no identifier at all.
    fn osc_identifier(&mut self, b: u8) {
        if b == b';' {
            let code = std::str::from_utf8(&self.osc_digits)
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(0);
            self.osc_code = Some(code.min(255) as u8);
            self.osc_digits.clear();
            return;
        }
        if b.is_ascii_digit() && self.osc_digits.len() < 3 {
            self.osc_digits.push(b);
            return;
        }
        // Not an identifier: everything so far becomes the payload.
        if self.osc_buf.len() < MAX_OSC {
            self.osc_buf.extend_from_slice(&self.osc_digits);
            self.osc_buf.push(b);
        }
        self.osc_digits.clear();
        self.osc_code = Some(0);
    }

    fn finish_osc(&mut self, out: &mut Vec<Action>) {
        let code = self.osc_code.take().unwrap_or(0);
        let data = String::from_utf8_lossy(&self.osc_buf).into_owned();
        self.osc_digits.clear();
        self.osc_buf.clear();
        self.state = State::Ground;
        out.push(Action::Osc { code, data });
    }

    fn string(&mut self, b: u8, out: &mut Vec<Action>) {
        if self.state == State::StringIgnoreEscape {
            self.state = State::StringIgnore;
            if b == b'\\' {
                self.state = State::Ground;
                out.push(Action::Ignored);
            }
            return;
        }
        if b == 0x1b {
            self.state = State::StringIgnoreEscape;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Action> {
        Parser::new().feed(bytes)
    }

    #[test]
    fn plain_text_and_newlines() {
        assert_eq!(
            parse(b"ab\r\nc"),
            vec![
                Action::Print('a'),
                Action::Print('b'),
                Action::Cr,
                Action::Lf,
                Action::Print('c')
            ]
        );
        assert_eq!(parse(b"\x07"), vec![Action::Bell]);
        assert_eq!(parse(b"\x08"), vec![Action::Backspace]);
        assert_eq!(parse(b"\t"), vec![Action::Tab(1)]);
        assert_eq!(parse(b"\x0b"), vec![Action::VTab]);
    }

    #[test]
    fn sgr_with_and_without_parameters() {
        match &parse(b"\x1b[1;31m")[0] {
            Action::Csi(p) => {
                assert_eq!(p.final_byte, b'm');
                assert_eq!(p.groups, vec![vec![1], vec![31]]);
                assert_eq!(p.get(0, 7), 1);
                assert_eq!(p.get(1, 7), 31);
                assert!(!p.is_set(2));
            }
            other => panic!("{other:?}"),
        }
        // A bare `m` still has one (defaulted) parameter slot, which is how
        // `CSI m` stays distinguishable from `CSI 1m` even though both mean
        // "reset" to an SGR handler.
        match &parse(b"\x1b[m")[0] {
            Action::Csi(p) => {
                assert_eq!(p.groups, vec![Vec::<u16>::new()]);
                assert!(p.is_set(0), "the slot exists");
                assert_eq!(p.get_raw(0, 99), 99, "but carries no value");
                assert!(!p.is_set(1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_parameters_keep_their_slot() {
        match &parse(b"\x1b[;5H")[0] {
            Action::Csi(p) => {
                assert_eq!(p.groups, vec![Vec::<u16>::new(), vec![5]]);
                assert_eq!(p.get(0, 1), 1);
                assert_eq!(p.get(1, 1), 5);
                assert!(p.is_set(0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn colon_sub_parameters_nest_inside_one_group() {
        match &parse(b"\x1b[4:3H")[0] {
            Action::Csi(p) => {
                assert_eq!(p.groups, vec![vec![4, 3]]);
                assert_eq!(p.sub(0, 0), Some(4));
                assert_eq!(p.sub(0, 1), Some(3));
            }
            other => panic!("{other:?}"),
        }
        match &parse(b"\x1b[38:2:10:20:30m")[0] {
            Action::Csi(p) => assert_eq!(p.groups, vec![vec![38, 2, 10, 20, 30]]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn private_and_intermediate_bytes_are_kept() {
        match &parse(b"\x1b[?25h")[0] {
            Action::Csi(p) => {
                assert_eq!(p.private, Some(b'?'));
                assert_eq!(p.final_byte, b'h');
            }
            other => panic!("{other:?}"),
        }
        match &parse(b"\x1b[1 q")[0] {
            Action::Csi(p) => assert_eq!(p.intermediates, vec![b' ']),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn osc_titles() {
        assert_eq!(parse(b"\x1b]0;my title\x07"), vec![Action::Osc { code: 0, data: "my title".into() }]);
        assert_eq!(parse(b"\x1b]2;other\x1b\\"), vec![Action::Osc { code: 2, data: "other".into() }]);
    }

    #[test]
    fn osc_133_shell_marks_parse() {
        assert_eq!(
            parse(b"\x1b]133;A\x07\x1b]133;D;0\x07"),
            vec![
                Action::Osc { code: 133, data: "A".into() },
                Action::Osc { code: 133, data: "D;0".into() }
            ]
        );
    }

    #[test]
    fn osc_without_an_identifier_keeps_its_payload() {
        match &parse(b"\x1b]weird\x07")[0] {
            Action::Osc { code, data } => {
                assert_eq!(*code, 0);
                assert_eq!(data, "weird");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn osc_embedded_escape_is_payload() {
        // Only BEL or ESC \ terminates an OSC.
        match &parse(b"\x1b]0;weird\x1bx\x07")[0] {
            Action::Osc { code, data } => {
                assert_eq!(*code, 0);
                assert!(data.starts_with("weird"), "{data:?}");
                assert!(data.ends_with('x'), "{data:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sequences_and_characters_split_across_feeds() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b[").is_empty());
        assert!(p.feed(b"3").is_empty());
        assert!(matches!(p.feed(b"1m")[0], Action::Csi(_)));

        let mut p = Parser::new();
        let bytes = "→".as_bytes();
        assert!(p.feed(&bytes[..1]).is_empty());
        assert_eq!(p.feed(&bytes[1..]), vec![Action::Print('→')]);

        let mut p = Parser::new();
        assert!(p.feed(b"\x1b]0;ti").is_empty());
        assert_eq!(p.feed(b"tle\x07"), vec![Action::Osc { code: 0, data: "title".into() }]);
    }

    #[test]
    fn invalid_utf8_becomes_a_replacement_not_a_panic() {
        assert_eq!(parse(&[0xff]), vec![Action::Print('\u{fffd}')]);
        assert_eq!(parse(&[0x80]), vec![Action::Print('\u{fffd}')]);
        assert_eq!(parse("é→".as_bytes()), vec![Action::Print('é'), Action::Print('→')]);
        // A scalar split by a `read` boundary is emitted only once we know it
        // was truncated, so `flush` exists.
        let mut p = Parser::new();
        assert!(p.feed(&[0xc3]).is_empty());
        assert_eq!(p.feed(b"a"), vec![Action::Print('\u{fffd}'), Action::Print('a')]);
        let mut p = Parser::new();
        assert!(p.feed(&[0xc3]).is_empty());
        assert_eq!(p.flush(), vec![Action::Print('\u{fffd}')]);
        // And the byte after a broken sequence is not swallowed: 0xf0 starts a
        // four-byte scalar, 0x28 cannot continue it, so '(' is re-processed as
        // the printable character it is.
        assert_eq!(
            parse(&[0xf0, 0x28, b'a']),
            vec![Action::Print('\u{fffd}'), Action::Print('('), Action::Print('a')]
        );
    }

    #[test]
    fn device_status_reports_are_surfaced() {
        assert!(matches!(parse(b"\x1b[6n")[0], Action::DeviceReport(_)));
    }

    #[test]
    fn string_sequences_run_to_st() {
        let actions = parse(b"\x1bPq#0;2;0;0;0\x1b\\after");
        assert_eq!(actions[0], Action::Ignored);
        assert_eq!(actions[1], Action::Print('a'));
    }

    #[test]
    fn esc_forms() {
        assert_eq!(parse(b"\x1b7"), vec![Action::Esc(b'7')]);
        assert_eq!(parse(b"\x1bD"), vec![Action::Esc(b'D')]);
        assert_eq!(parse(b"\x1bM"), vec![Action::Esc(b'M')]);
        // `ESC ( B` designates a charset: two bytes, one action.
        assert_eq!(parse(b"\x1b(B"), vec![Action::CharsetDesignate]);
    }

    #[test]
    fn reset_clears_partial_state() {
        let mut p = Parser::new();
        assert!(p.feed(b"\x1b]0;partial").is_empty());
        p.reset();
        assert_eq!(p.feed(b"ok"), vec![Action::Print('o'), Action::Print('k')]);
    }
}
