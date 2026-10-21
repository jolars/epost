# Local patch

Source: crates.io `tui-textarea-2` 0.11.0, from
https://github.com/srothgan/tui-textarea. The upstream MIT license is in LICENSE.

`src/screen_map.rs::screen_map_load` appends an empty visual fragment for the
active insertion cursor when its final fragment fills the wrap width. This
gives the caret a visible cell at the end of the logical line. Other full
lines retain their usual spacing. `src/textarea.rs::move_cursor_with_shift`
refreshes the mapping when entering or leaving EOL. Message text is unchanged.

Word wrapping also omits overflowing separator whitespace at the start of
continuations. Logical offsets still include those source characters; the
screen map places their cursor at the next word, and backward movement
traverses them without changing the message. Initial indentation, trailing
whitespace, glyph wrapping, and unwrapped text retain their behavior.

The regression tests live in `src/ui/compose_body.rs` in the application.
Remove the Cargo patch and this directory when an upstream release includes
the fixes.
