def rw(p, f):
    s = open(p, encoding='utf-8', newline='').read()
    nl = '\r\n' if '\r\n' in s else '\n'
    s2 = f(s, nl)
    assert s2 != s, p
    open(p, 'w', encoding='utf-8', newline='').write(s2)


def render(s, nl):
    s = s.replace(
        "        if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {" + nl +
        "            out.push_str(&format!(\"\\\\x{cp:02X}\"));" + nl +
        "        } else {",
        "        if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {" + nl +
        "            out.push_str(&format!(\"\\\\x{cp:02X}\"));" + nl +
        "        } else if is_bidi_spoofing_control(cp) {" + nl +
        "            out.push_str(&format!(\"\\\\u{{{cp:04X}}}\"));" + nl +
        "        } else {", 1)
    s = s.replace(
        "/// `SqlValueJson`'s wire shape -> a display string.",
        "/// Bidirectional *embedding / override / isolate* controls (U+202A..U+202E,\n"
        "/// U+2066..U+2069). They reorder what a terminal displays (\"Trojan Source\"\n"
        "/// style spoofing of row content, e.g. a value that *shows* as one thing and\n"
        "/// copy-pastes as another), so they are made visible as `\\u{XXXX}` exactly like\n"
        "/// control characters. The plain direction *marks* LRM/RLM/ALM and every letter\n"
        "/// of right-to-left scripts pass through untouched.\n"
        "fn is_bidi_spoofing_control(cp: u32) -> bool {\n"
        "    (0x202A..=0x202E).contains(&cp) || (0x2066..=0x2069).contains(&cp)\n"
        "}\n\n"
        "/// `SqlValueJson`'s wire shape -> a display string.", 1)
    s = s.replace(nl.join(["    #[test]", "    fn sanitizes_ansi_escape_sequences() {"]),
                  nl.join([
                      "    #[test]",
                      "    fn bidi_override_embedding_and_isolate_controls_are_made_visible() {",
                      "        for cp in (0x202Au32..=0x202E).chain(0x2066..=0x2069) {",
                      "            let c = char::from_u32(cp).unwrap();",
                      "            let safe = sanitize_for_terminal(&format!(\"a{c}b\"));",
                      "            assert!(!safe.contains(c), \"U+{cp:04X} must not reach the terminal\");",
                      "            assert!(safe.contains(&format!(\"\\\\u{{{cp:04X}}}\")), \"{safe}\");",
                      "        }",
                      "        // Right-to-left *text* and the plain marks are untouched.",
                      "        let s = \"\\u{05E9}\\u{05DC}\\u{05D5}\\u{05DD} \\u{0645}\\u{0631}\\u{062D}\\u{0628}\\u{0627} \\u{200E}x\\u{200F}\";",
                      "        assert_eq!(sanitize_for_terminal(s), s);",
                      "    }",
                      "",
                      "    #[test]",
                      "    fn sanitizes_ansi_escape_sequences() {"]), 1)
    return s


def main_rs(s, nl):
    old = ("fn read_script_file(path: &str) -> Result<String, String> {" + nl +
           "    let mut f = std::fs::File::open(path).map_err(|e| format!(\"could not open {path}: {e}\"))?;")
    new = ("fn read_script_file(path: &str) -> Result<String, String> {" + nl +
           "    // Only regular files: opening a device (`CON`, `NUL`, a named pipe) as a" + nl +
           "    // \"script\" would block forever waiting for input that never comes." + nl +
           "    match std::fs::metadata(path) {" + nl +
           "        Ok(m) if m.is_file() => {}" + nl +
           "        Ok(_) => return Err(format!(\"{path} is not a regular file\"))," + nl +
           "        Err(e) => return Err(format!(\"could not open {path}: {e}\"))," + nl +
           "    }" + nl +
           "    let mut f = std::fs::File::open(path).map_err(|e| format!(\"could not open {path}: {e}\"))?;")
    assert old in s
    return s.replace(old, new, 1)


rw(r'E:\RubiXDb\cli\src\render.rs', render)
rw(r'E:\RubiXDb\cli\src\main.rs', main_rs)
print("ok")
