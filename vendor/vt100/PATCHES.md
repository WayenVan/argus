# Local patches to vt100 0.16.2

Upstream: https://github.com/doy/vt100-rust

- `src/row.rs`: `Row::resize` and `Row::truncate` blank a wide character
  whose second half they cut off. Without this, shrinking the screen
  (`Screen::set_size`) or inserting characters (ICH) with a wide character
  at the new right edge leaves a wide cell with no continuation, and the
  next write or erase there panics (`screen.rs:870` unwrap on `None`,
  `row.rs:89` index out of bounds). Covered by the argus test
  `wide_char_cut_by_a_shrink_does_not_panic` in
  `crates/argus/src/manager/screen.rs`.
