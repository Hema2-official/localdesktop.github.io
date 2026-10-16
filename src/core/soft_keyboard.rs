//! What Android's keyboard is told about a program's text field: the field's kind, as the program
//! says it in text-input-v3 (a purpose and hints), made into Android's `EditorInfo` input type and
//! IME options, so that a password field gets no suggestions and a number field digits.

/// text-input-v3's `content_hint` bits.
pub mod hint {
    pub const COMPLETION: u32 = 0x1;
    pub const SPELLCHECK: u32 = 0x2;
    pub const AUTO_CAPITALIZATION: u32 = 0x4;
    pub const UPPERCASE: u32 = 0x10;
    pub const TITLECASE: u32 = 0x20;
    pub const HIDDEN_TEXT: u32 = 0x40;
    pub const SENSITIVE_DATA: u32 = 0x80;
    pub const LATIN: u32 = 0x100;
    pub const MULTILINE: u32 = 0x200;
}

/// text-input-v3's `content_purpose` values.
pub mod purpose {
    pub const NORMAL: u32 = 0;
    pub const DIGITS: u32 = 2;
    pub const NUMBER: u32 = 3;
    pub const PHONE: u32 = 4;
    pub const URL: u32 = 5;
    pub const EMAIL: u32 = 6;
    pub const NAME: u32 = 7;
    pub const PASSWORD: u32 = 8;
    pub const PIN: u32 = 9;
    pub const DATE: u32 = 10;
    pub const TIME: u32 = 11;
    pub const DATETIME: u32 = 12;
    pub const TERMINAL: u32 = 13;
}

// android.text.InputType
const CLASS_TEXT: i32 = 0x1;
const CLASS_NUMBER: i32 = 0x2;
const CLASS_PHONE: i32 = 0x3;
const CLASS_DATETIME: i32 = 0x4;
const TEXT_CAP_CHARACTERS: i32 = 0x1000;
const TEXT_CAP_WORDS: i32 = 0x2000;
const TEXT_CAP_SENTENCES: i32 = 0x4000;
const TEXT_AUTO_CORRECT: i32 = 0x8000;
const TEXT_AUTO_COMPLETE: i32 = 0x1_0000;
const TEXT_MULTI_LINE: i32 = 0x2_0000;
const TEXT_NO_SUGGESTIONS: i32 = 0x8_0000;
const TEXT_URI: i32 = 0x10;
const TEXT_EMAIL_ADDRESS: i32 = 0x20;
const TEXT_PERSON_NAME: i32 = 0x60;
const TEXT_PASSWORD: i32 = 0x80;
const TEXT_VISIBLE_PASSWORD: i32 = 0x90;
const NUMBER_SIGNED: i32 = 0x1000;
const NUMBER_DECIMAL: i32 = 0x2000;
const NUMBER_PASSWORD: i32 = 0x10;
const DATETIME_DATE: i32 = 0x10;
const DATETIME_TIME: i32 = 0x20;

// android.view.inputmethod.EditorInfo
const IME_ACTION_NONE: i32 = 0x1;
const IME_FLAG_NO_PERSONALIZED_LEARNING: i32 = 0x100_0000;
const IME_FLAG_NO_FULLSCREEN: i32 = 0x200_0000;
const IME_FLAG_NO_EXTRACT_UI: i32 = 0x1000_0000;
const IME_FLAG_NO_ENTER_ACTION: i32 = 0x4000_0000;
const IME_FLAG_FORCE_ASCII: i32 = i32::MIN; // 0x8000_0000

/// Android's input type and IME options for a field with these hints and purpose. The keyboard
/// stays a keyboard (never a full-screen editor), and its Enter key sends Enter: the program
/// decides what that does.
pub fn editor_info(hints: u32, purpose: u32) -> (i32, i32) {
    let has = |bit| hints & bit != 0;
    let mut options = IME_ACTION_NONE
        | IME_FLAG_NO_ENTER_ACTION
        | IME_FLAG_NO_EXTRACT_UI
        | IME_FLAG_NO_FULLSCREEN;
    if has(hint::SENSITIVE_DATA) || purpose == purpose::PASSWORD || purpose == purpose::PIN {
        options |= IME_FLAG_NO_PERSONALIZED_LEARNING;
    }
    if has(hint::LATIN) {
        options |= IME_FLAG_FORCE_ASCII;
    }
    let input_type = match purpose {
        purpose::DIGITS => CLASS_NUMBER,
        purpose::NUMBER => CLASS_NUMBER | NUMBER_SIGNED | NUMBER_DECIMAL,
        purpose::PHONE => CLASS_PHONE,
        purpose::PIN => CLASS_NUMBER | NUMBER_PASSWORD,
        purpose::DATE => CLASS_DATETIME | DATETIME_DATE,
        purpose::TIME => CLASS_DATETIME | DATETIME_TIME,
        purpose::DATETIME => CLASS_DATETIME,
        // Suggestions and corrections would rewrite commands.
        purpose::TERMINAL => CLASS_TEXT | TEXT_VISIBLE_PASSWORD | TEXT_NO_SUGGESTIONS,
        _ => {
            let variation = match purpose {
                purpose::URL => TEXT_URI,
                purpose::EMAIL => TEXT_EMAIL_ADDRESS,
                purpose::NAME => TEXT_PERSON_NAME,
                purpose::PASSWORD => TEXT_PASSWORD,
                _ if has(hint::HIDDEN_TEXT) => TEXT_PASSWORD,
                _ => 0,
            };
            let mut flags = 0;
            if has(hint::UPPERCASE) {
                flags |= TEXT_CAP_CHARACTERS;
            } else if has(hint::TITLECASE) {
                flags |= TEXT_CAP_WORDS;
            } else if has(hint::AUTO_CAPITALIZATION) {
                flags |= TEXT_CAP_SENTENCES;
            }
            if has(hint::SPELLCHECK) {
                flags |= TEXT_AUTO_CORRECT;
            }
            if has(hint::COMPLETION) {
                flags |= TEXT_AUTO_COMPLETE;
            }
            if has(hint::MULTILINE) {
                flags |= TEXT_MULTI_LINE;
            }
            CLASS_TEXT | variation | flags
        }
    };
    (input_type, options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_describe_fields_to_android() {
        let (text, options) = editor_info(0, purpose::NORMAL);
        assert_eq!(text, CLASS_TEXT);
        assert_ne!(options & IME_FLAG_NO_EXTRACT_UI, 0);
        assert_ne!(options & IME_FLAG_NO_ENTER_ACTION, 0);
        assert_eq!(options & IME_FLAG_NO_PERSONALIZED_LEARNING, 0);

        let (password, options) = editor_info(hint::HIDDEN_TEXT, purpose::PASSWORD);
        assert_eq!(password, CLASS_TEXT | TEXT_PASSWORD);
        assert_ne!(options & IME_FLAG_NO_PERSONALIZED_LEARNING, 0);
        assert_eq!(
            editor_info(hint::HIDDEN_TEXT, purpose::NORMAL).0,
            CLASS_TEXT | TEXT_PASSWORD
        );

        assert_eq!(editor_info(0, purpose::DIGITS).0, CLASS_NUMBER);
        assert_eq!(
            editor_info(0, purpose::PIN).0,
            CLASS_NUMBER | NUMBER_PASSWORD
        );
        assert_eq!(
            editor_info(0, purpose::EMAIL).0,
            CLASS_TEXT | TEXT_EMAIL_ADDRESS
        );
        assert_eq!(
            editor_info(0, purpose::TERMINAL).0 & TEXT_NO_SUGGESTIONS,
            TEXT_NO_SUGGESTIONS
        );
        assert_eq!(
            editor_info(hint::AUTO_CAPITALIZATION | hint::MULTILINE, purpose::NORMAL).0,
            CLASS_TEXT | TEXT_CAP_SENTENCES | TEXT_MULTI_LINE
        );
        assert_eq!(
            editor_info(hint::UPPERCASE | hint::AUTO_CAPITALIZATION, purpose::NORMAL).0,
            CLASS_TEXT | TEXT_CAP_CHARACTERS
        );
        assert_eq!(
            editor_info(hint::LATIN, purpose::NORMAL).1 & IME_FLAG_FORCE_ASCII,
            IME_FLAG_FORCE_ASCII
        );
    }
}
