//! Percent-encoding of UTF-8 query bytes and the ASCII fold the last
//! waterfall tier applies to the general search query.

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode every byte outside the RFC 3986 unreserved set with
/// uppercase hex. Non-ASCII input encodes as its UTF-8 bytes.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    out
}

/// Fold `input` to plain ASCII: Latin diacritics lose their marks at
/// the same case, typographic punctuation maps to its ASCII twin, and
/// anything without an honest ASCII spelling (CJK, emoji, …) drops
/// out. Returns `None` when nothing survives — an empty `q=` is never
/// a real query.
pub fn ascii_fold(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            out.push_str(fold_char(c));
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// One non-ASCII character's ASCII spelling; `""` when it has none.
fn fold_char(c: char) -> &'static str {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ȁ' | 'ȃ' | 'ạ' | 'ả' | 'ấ' | 'ầ'
        | 'ẩ' | 'ẫ' | 'ậ' | 'ắ' | 'ằ' | 'ẳ' | 'ẵ' | 'ặ' => "a",
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' | 'Ȁ' | 'Ȃ' | 'Ạ' | 'Ả' | 'Ấ' | 'Ầ'
        | 'Ẩ' | 'Ẫ' | 'Ậ' | 'Ắ' | 'Ằ' | 'Ẳ' | 'Ẵ' | 'Ặ' => "A",
        'æ' | 'ǽ' => "ae",
        'Æ' | 'Ǽ' => "AE",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => "C",
        'ď' | 'đ' | 'ð' => "d",
        'Ď' | 'Đ' | 'Ð' => "D",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'ȅ' | 'ȇ' | 'ẹ' | 'ẻ' | 'ẽ' | 'ế'
        | 'ề' | 'ể' | 'ễ' | 'ệ' => "e",
        'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' | 'Ȅ' | 'Ȇ' | 'Ẹ' | 'Ẻ' | 'Ẽ' | 'Ế'
        | 'Ề' | 'Ể' | 'Ễ' | 'Ệ' => "E",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => "G",
        'ĥ' | 'ħ' => "h",
        'Ĥ' | 'Ħ' => "H",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'ị' | 'ỉ' => "i",
        'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' | 'Ị' | 'Ỉ' => "I",
        'ĵ' => "j",
        'Ĵ' => "J",
        'ķ' => "k",
        'Ķ' => "K",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'Ĺ' | 'Ļ' | 'Ľ' | 'Ŀ' | 'Ł' => "L",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
        'Ñ' | 'Ń' | 'Ņ' | 'Ň' | 'Ŋ' => "N",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ȍ' | 'ȏ' | 'ọ' | 'ỏ' | 'ố' | 'ồ'
        | 'ổ' | 'ỗ' | 'ộ' | 'ớ' | 'ờ' | 'ở' | 'ỡ' | 'ợ' | 'ơ' => "o",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' | 'Ȍ' | 'Ȏ' | 'Ọ' | 'Ỏ' | 'Ố' | 'Ồ'
        | 'Ổ' | 'Ỗ' | 'Ộ' | 'Ớ' | 'Ờ' | 'Ở' | 'Ỡ' | 'Ợ' | 'Ơ' => "O",
        'œ' => "oe",
        'Œ' => "OE",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'Ŕ' | 'Ŗ' | 'Ř' => "R",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' | 'ſ' => "s",
        'Ś' | 'Ŝ' | 'Ş' | 'Š' | 'Ș' => "S",
        'ß' => "ss",
        'ẞ' => "SS",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'Ţ' | 'Ť' | 'Ŧ' | 'Ț' => "T",
        'þ' => "th",
        'Þ' => "TH",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ụ' | 'ủ' | 'ứ' | 'ừ' | 'ử'
        | 'ữ' | 'ự' | 'ư' => "u",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' | 'Ụ' | 'Ủ' | 'Ứ' | 'Ừ' | 'Ử'
        | 'Ữ' | 'Ự' | 'Ư' => "U",
        'ŵ' => "w",
        'Ŵ' => "W",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'Ý' | 'Ŷ' | 'Ÿ' => "Y",
        'ź' | 'ż' | 'ž' => "z",
        'Ź' | 'Ż' | 'Ž' => "Z",
        'ĳ' => "ij",
        'Ĳ' => "IJ",
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '`' | '´' | '′' => "'",
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '«' | '»' | '″' => "\"",
        '‐' | '‑' | '‒' | '–' | '—' | '―' | '−' => "-",
        '…' => "...",
        '·' | '•' | '․' | '‧' => " ",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreserved_passes_through() {
        assert_eq!(percent_encode("abcXYZ-._~019"), "abcXYZ-._~019");
    }

    #[test]
    fn space_and_reserved_encode() {
        assert_eq!(percent_encode("a b&c+d=e/f?"), "a%20b%26c%2Bd%3De%2Ff%3F");
    }

    #[test]
    fn non_latin_encodes_utf8_bytes() {
        assert_eq!(percent_encode("夜の歌"), "%E5%A4%9C%E3%81%AE%E6%AD%8C");
    }

    #[test]
    fn fold_drops_marks_and_keeps_case() {
        assert_eq!(ascii_fold("Sigur Rós").as_deref(), Some("Sigur Ros"));
        assert_eq!(ascii_fold("Beyoncé").as_deref(), Some("Beyonce"));
        assert_eq!(ascii_fold("Þórunn").as_deref(), Some("THorunn"));
        assert_eq!(ascii_fold("æon").as_deref(), Some("aeon"));
        assert_eq!(ascii_fold("Groß").as_deref(), Some("Gross"));
    }

    #[test]
    fn fold_typographic_punctuation() {
        assert_eq!(ascii_fold("it’s — fine").as_deref(), Some("it's - fine"));
    }

    #[test]
    fn fold_cjk_has_no_spelling() {
        assert_eq!(ascii_fold("夜の歌"), None);
        assert_eq!(ascii_fold("L’été 夜").as_deref(), Some("L'ete"));
    }

    #[test]
    fn fold_pure_ascii_is_unchanged() {
        assert_eq!(ascii_fold("Plain Title").as_deref(), Some("Plain Title"));
    }
}
