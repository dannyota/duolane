pub fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes)]
}

pub fn terminal_text(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}
