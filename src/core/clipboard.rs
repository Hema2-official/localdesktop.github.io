//! What sharing the clipboard between Android and the desktop has to decide, apart from both of
//! them: which types to offer and ask for, and when to copy which way. The Android side is
//! `android::clipboard`, the desktop side `android::guest::clipboard`.

/// On the selections the link itself puts on the desktop, so that it doesn't take them for the
/// desktop's own and copy them back.
pub const LINK_TYPE: &str = "application/x-localdesktop-clipboard";

/// How much text Android gets at most, in UTF-16 units as it goes through Binder, whose
/// transactions end at 1 MB.
pub const ANDROID_TEXT_LIMIT: usize = 250_000;

/// How much of a desktop selection is read at most: the most text Android takes, as UTF-8.
pub const DESKTOP_READ_LIMIT: usize = ANDROID_TEXT_LIMIT * 3;

const HTML_TYPE: &str = "text/html";
/// What a text is offered as on the desktop, the UTF-8 ones first. `STRING` is Latin-1 by
/// X11's rules; the others are UTF-8 on every desktop of this decade.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// What the link carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
}

/// What a clip on Android has for the desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kinds {
    pub text: bool,
    pub html: bool,
}

impl Kinds {
    pub fn any(self) -> bool {
        self.text || self.html
    }
}

fn same_type(a: &str, b: &str) -> bool {
    // "text/plain; charset=utf-8" and "TEXT/PLAIN;charset=UTF-8" are the same type.
    let normal = |it: &str| {
        it.chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    normal(a) == normal(b)
}

/// The kind of content a desktop type stands for, if the link carries it.
pub fn kind_of(mime_type: &str) -> Option<Kind> {
    if same_type(mime_type, HTML_TYPE) {
        Some(Kind::Html)
    } else if TEXT_TYPES.iter().any(|it| same_type(mime_type, it)) {
        Some(Kind::Text)
    } else {
        None
    }
}

/// The types to offer on the desktop for a clip from Android.
pub fn desktop_types(kinds: Kinds) -> Vec<&'static str> {
    let mut types = Vec::new();
    if kinds.html {
        types.push(HTML_TYPE);
    }
    if kinds.any() {
        // Android makes a text of every HTML clip.
        types.extend(TEXT_TYPES);
    }
    if !types.is_empty() {
        types.push(LINK_TYPE);
    }
    types
}

/// Whether a selection offering these types is one the link put there.
pub fn is_from_link<T: AsRef<str>>(offered: &[T]) -> bool {
    offered.iter().any(|it| it.as_ref() == LINK_TYPE)
}

/// The type to ask a desktop selection for to get its text, the best one it offers.
pub fn text_type<T: AsRef<str>>(offered: &[T]) -> Option<&str> {
    TEXT_TYPES.iter().find_map(|wanted| {
        offered
            .iter()
            .map(AsRef::as_ref)
            .find(|it| same_type(it, wanted))
    })
}

/// The type to ask a desktop selection for to get its HTML.
pub fn html_type<T: AsRef<str>>(offered: &[T]) -> Option<&str> {
    offered
        .iter()
        .map(AsRef::as_ref)
        .find(|it| same_type(it, HTML_TYPE))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// The text in what a desktop program sent as `mime_type`. Programs send UTF-8 unless the type
/// says otherwise, some (Firefox under X11) HTML as UTF-16 with a byte order mark.
pub fn decode(mime_type: &str, bytes: &[u8]) -> String {
    let text = match bytes {
        [0xff, 0xfe, rest @ ..] => utf16(rest, false),
        [0xfe, 0xff, rest @ ..] => utf16(rest, true),
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ if same_type(mime_type, "STRING") => latin1(bytes),
        _ => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_owned(),
            Err(_) if kind_of(mime_type) == Some(Kind::Text) => latin1(bytes),
            Err(_) => String::from_utf8_lossy(bytes).into_owned(),
        },
    };
    // C programs end their strings with one; Android would show it.
    text.trim_end_matches('\0').to_owned()
}

/// What to send a desktop program that asked for `mime_type`.
pub fn encode(mime_type: &str, text: &str) -> Vec<u8> {
    if same_type(mime_type, "STRING") {
        text.chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .collect()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Whether Android takes a clip of this text and HTML.
pub fn fits_android(text: &str, html: Option<&str>) -> bool {
    let units = text.encode_utf16().count() + html.map_or(0, |it| it.encode_utf16().count());
    units <= ANDROID_TEXT_LIMIT
}

/// What the link does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Put Android's clip on the desktop, to be read when a program there asks for it.
    OfferToDesktop(Kinds),
    /// Read the desktop's selection and make it Android's clip.
    CopyToAndroid,
}

/// A clip on Android, as far as Android tells without it being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AndroidClip {
    /// Changes with every clip: when it was made, or a count of the changes.
    pub stamp: i64,
    /// The app made it itself, of a desktop selection.
    pub own: bool,
    pub kinds: Kinds,
}

/// Why Android's clip on offer on the desktop couldn't be read when a program there asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unread {
    /// It has no text after all: its items are empty, or not text.
    Empty,
    /// Android gave none although the window has focus: the clipboard was cleared (Android 13
    /// and later do that after an hour, apps can too).
    Gone,
    /// The window doesn't have focus, and Android only lets the app with focus read.
    Unfocused,
    /// Reading failed: the exception Android threw, or "JNI".
    Failed(String),
}

impl Unread {
    /// Whether the clip can't be read later either. Then the offer is withdrawn, or every
    /// program on the desktop that asks for it would get nothing, and the link would ask Android
    /// again each time.
    pub fn lasting(&self) -> bool {
        !matches!(self, Self::Unfocused)
    }
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("it has no text"),
            Self::Gone => f.write_str("Android gave none"),
            Self::Unfocused => f.write_str("the window doesn't have focus"),
            Self::Failed(what) => write!(f, "reading it failed ({what})"),
        }
    }
}

/// When to copy which way. The latest copy wins, wherever it was made.
///
/// Android only shows its clipboard to the app whose window has focus, so its clips are looked
/// at when the window gets it. The desktop's selections are copied to Android when the window
/// loses it: until then nothing on Android could paste them, and copying inside the desktop
/// costs nothing (nor brings up Android's clipboard popup each time).
#[derive(Debug, Default)]
pub struct Sync {
    focused: bool,
    /// The clip on Android when the link last looked.
    seen: Option<i64>,
    /// The desktop has a selection of its own that Android doesn't have yet.
    pending: bool,
}

impl Sync {
    pub fn new(focused: bool) -> Self {
        Self {
            focused,
            ..Self::default()
        }
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    /// The window got or lost focus. After getting it, look at Android's clip (`android_clip`).
    pub fn focus(&mut self, focused: bool) -> Step {
        let lost = self.focused && !focused;
        self.focused = focused;
        if lost && self.pending {
            self.pending = false;
            return Step::CopyToAndroid;
        }
        Step::Nothing
    }

    /// What is on Android's clipboard now, `None` if nothing or if Android doesn't say.
    pub fn android_clip(&mut self, clip: Option<AndroidClip>) -> Step {
        let Some(clip) = clip else {
            return Step::Nothing;
        };
        if self.seen == Some(clip.stamp) {
            return Step::Nothing;
        }
        self.seen = Some(clip.stamp);
        if clip.own || !clip.kinds.any() {
            return Step::Nothing;
        }
        // Newer than whatever the desktop has.
        self.pending = false;
        Step::OfferToDesktop(clip.kinds)
    }

    /// The desktop's selection changed to one offering these types, or to none.
    pub fn desktop_selection<T: AsRef<str>>(&mut self, offered: Option<&[T]>) -> Step {
        let Some(offered) = offered else {
            self.pending = false;
            return Step::Nothing;
        };
        if is_from_link(offered) {
            return Step::Nothing;
        }
        if text_type(offered).is_none() && html_type(offered).is_none() {
            self.pending = false;
            return Step::Nothing;
        }
        if self.focused {
            self.pending = true;
            Step::Nothing
        } else {
            Step::CopyToAndroid
        }
    }

    /// The session's compositor is gone, and its selection with it.
    pub fn desktop_gone(&mut self) {
        self.pending = false;
    }

    /// Android's clip has to be looked at again, e.g. for a new desktop session to get it.
    pub fn forget_android_clip(&mut self) {
        self.seen = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: Kinds = Kinds {
        text: true,
        html: false,
    };

    fn foreign(stamp: i64) -> Option<AndroidClip> {
        Some(AndroidClip {
            stamp,
            own: false,
            kinds: TEXT,
        })
    }

    #[test]
    fn should_tell_the_types_it_carries() {
        assert_eq!(kind_of("text/plain;charset=utf-8"), Some(Kind::Text));
        assert_eq!(kind_of("TEXT/PLAIN; charset=UTF-8"), Some(Kind::Text));
        assert_eq!(kind_of("UTF8_STRING"), Some(Kind::Text));
        assert_eq!(kind_of("text/html"), Some(Kind::Html));
        assert_eq!(kind_of("image/png"), None);
        assert_eq!(kind_of(LINK_TYPE), None);
    }

    #[test]
    fn should_ask_for_the_best_text_offered() {
        let offered = ["STRING", "text/plain", "text/plain;charset=utf-8", "text/html"];
        assert_eq!(text_type(&offered), Some("text/plain;charset=utf-8"));
        assert_eq!(html_type(&offered), Some("text/html"));
        assert_eq!(text_type(&["TEXT", "STRING"]), Some("TEXT"));
        assert_eq!(text_type(&["image/png"]), None);
        assert_eq!(html_type(&["text/plain"]), None);
    }

    #[test]
    fn should_offer_text_for_html_and_mark_its_offers() {
        let types = desktop_types(Kinds {
            text: false,
            html: true,
        });
        assert_eq!(types.first(), Some(&"text/html"));
        assert!(types.contains(&"text/plain;charset=utf-8"));
        assert!(is_from_link(&types));
        assert!(!desktop_types(TEXT).contains(&"text/html"));
        assert!(desktop_types(Kinds::default()).is_empty());
        assert!(!is_from_link(&["text/plain"]));
    }

    #[test]
    fn should_decode_what_programs_send() {
        assert_eq!(decode("text/plain;charset=utf-8", "árvíz".as_bytes()), "árvíz");
        assert_eq!(decode("STRING", &[0xe1, b'r']), "ár");
        assert_eq!(decode("text/plain", &[0xe1, b'r']), "ár");
        assert_eq!(decode("text/plain", b"text\0"), "text");
        assert_eq!(decode("text/html", &[0xff, 0xfe, b'<', 0, b'b', 0, b'>', 0]), "<b>");
        assert_eq!(decode("text/html", &[0xfe, 0xff, 0, b'<', 0, b'b', 0, b'>']), "<b>");
        assert_eq!(decode("text/html", &[0xef, 0xbb, 0xbf, b'<', b'b', b'>']), "<b>");
    }

    #[test]
    fn should_encode_for_the_type_asked() {
        assert_eq!(encode("UTF8_STRING", "ár"), "ár".as_bytes());
        assert_eq!(encode("STRING", "ár€"), vec![0xe1, b'r', b'?']);
    }

    #[test]
    fn should_keep_large_texts_from_android() {
        assert!(fits_android("text", Some("<b>text</b>")));
        assert!(fits_android(&"a".repeat(ANDROID_TEXT_LIMIT), None));
        assert!(!fits_android(&"a".repeat(ANDROID_TEXT_LIMIT), Some("a")));
        // Counted as Android stores them.
        assert!(fits_android(&"é".repeat(ANDROID_TEXT_LIMIT), None));
        assert!(!fits_android(&"😀".repeat(ANDROID_TEXT_LIMIT / 2 + 1), None));
    }

    #[test]
    fn should_offer_a_new_android_clip_once() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
        assert_eq!(sync.android_clip(foreign(1)), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);
        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(1)), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(2)), Step::OfferToDesktop(TEXT));
    }

    #[test]
    fn should_leave_clips_it_cannot_carry_and_its_own() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.android_clip(None), Step::Nothing);
        let image = AndroidClip {
            stamp: 1,
            own: false,
            kinds: Kinds::default(),
        };
        assert_eq!(sync.android_clip(Some(image)), Step::Nothing);
        let own = AndroidClip {
            stamp: 2,
            own: true,
            kinds: TEXT,
        };
        assert_eq!(sync.android_clip(Some(own)), Step::Nothing);
    }

    #[test]
    fn should_withdraw_a_clip_it_cannot_read_later_either() {
        assert!(Unread::Empty.lasting());
        assert!(Unread::Gone.lasting());
        assert!(Unread::Failed("java.lang.SecurityException".into()).lasting());
        // Android lets the window read it once it has focus again.
        assert!(!Unread::Unfocused.lasting());

        // Withdrawn, it isn't offered again when the window gets focus, only the next clip is.
        let mut sync = Sync::new(true);
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
        assert_eq!(sync.focus(false), Step::Nothing);
        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(1)), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(2)), Step::OfferToDesktop(TEXT));
    }

    #[test]
    fn should_copy_to_android_when_the_window_loses_focus() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        assert_eq!(sync.desktop_selection(Some(&["UTF8_STRING"])), Step::Nothing);
        assert_eq!(sync.focus(false), Step::CopyToAndroid);
        // Only once.
        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);
    }

    #[test]
    fn should_copy_to_android_at_once_without_focus() {
        let mut sync = Sync::new(false);
        assert_eq!(
            sync.desktop_selection(Some(&["text/plain"])),
            Step::CopyToAndroid
        );
        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);
    }

    #[test]
    fn should_not_copy_its_own_offers_back() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
        let own = desktop_types(TEXT);
        assert_eq!(sync.desktop_selection(Some(&own)), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);
    }

    #[test]
    fn should_let_the_latest_copy_win() {
        let mut sync = Sync::new(true);
        // Copied on the desktop, then something arrives on Android before the window loses
        // focus (another device's clipboard, say).
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
        assert_eq!(sync.focus(false), Step::Nothing);

        // Copied on Android, then on the desktop.
        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.android_clip(foreign(2)), Step::OfferToDesktop(TEXT));
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        assert_eq!(sync.focus(false), Step::CopyToAndroid);
    }

    #[test]
    fn should_forget_a_selection_that_is_gone() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        assert_eq!(sync.desktop_selection(None::<&[&str]>), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);

        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        assert_eq!(sync.desktop_selection(Some(&["image/png"])), Step::Nothing);
        assert_eq!(sync.focus(false), Step::Nothing);

        assert_eq!(sync.focus(true), Step::Nothing);
        assert_eq!(sync.desktop_selection(Some(&["text/plain"])), Step::Nothing);
        sync.desktop_gone();
        assert_eq!(sync.focus(false), Step::Nothing);
    }

    #[test]
    fn should_offer_the_clip_again_to_a_new_session() {
        let mut sync = Sync::new(true);
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
        sync.desktop_gone();
        sync.forget_android_clip();
        assert_eq!(sync.android_clip(foreign(1)), Step::OfferToDesktop(TEXT));
    }
}
