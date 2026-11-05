use core::ops::Range;

use emath::TSTransform;
use epaint::text::{CharIndex, PlacedRow};

use crate::{Context, Galley, Id};

use super::{CCursorRange, text_cursor_state::is_word_char};

/// AccessKit's `word_starts` uses `u8` indices, so text runs cannot exceed this length.
pub(crate) const MAX_CHARS_PER_TEXT_RUN: usize = 255;

/// The text runs of a row, as ranges of its columns, in logical order.
///
/// A run is a stretch of chars with the same bidi level, so it reads in one direction and is
/// contiguous on screen, at most [`MAX_CHARS_PER_TEXT_RUN`] long.
/// The row's trailing newline, if any, belongs to the last run.
/// A row always has at least one run, even when empty.
pub(crate) fn row_text_runs(row: &PlacedRow) -> Vec<Range<usize>> {
    let glyph_count = row.glyphs.len();
    let total_chars = glyph_count + usize::from(row.ends_with_newline);
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..glyph_count {
        if row.glyphs[i].bidi_level != row.glyphs[i - 1].bidi_level
            || i - start == MAX_CHARS_PER_TEXT_RUN
        {
            runs.push(start..i);
            start = i;
        }
    }
    if total_chars - start > MAX_CHARS_PER_TEXT_RUN {
        runs.push(start..glyph_count);
        start = glyph_count;
    }
    runs.push(start..total_chars);
    runs
}

fn text_run_id(parent_id: Id, row: usize, run: usize) -> Id {
    parent_id.with(row).with(run)
}

/// Convert a (row, column) layout cursor position to a text run node ID and character index.
///
/// A column on the boundary between two runs is the start of the second run when the cursor
/// sticks to the char after it (`prefer_next_row`), and the end of the first one otherwise,
/// so the position names the run edge the cursor is drawn at.
fn text_run_position(
    parent_id: Id,
    galley: &Galley,
    row: usize,
    column: CharIndex,
    prefer_next_row: bool,
) -> accesskit::TextPosition {
    let column = column.0;
    let runs = galley.rows.get(row).map(row_text_runs).unwrap_or_default();
    let run_index = runs
        .iter()
        .position(|run| {
            if prefer_next_row {
                column < run.end
            } else {
                column <= run.end
            }
        })
        .unwrap_or_else(|| runs.len().saturating_sub(1));
    let run_start = runs.get(run_index).map_or(0, |run| run.start);
    accesskit::TextPosition {
        node: text_run_id(parent_id, row, run_index).accesskit_id(),
        character_index: column - run_start,
    }
}

/// Update accesskit with the current text state.
///
/// This adds the [`accesskit::Role::TextRun`] children of the widget, and the
/// current text selection. It never touches the role of the widget itself: that
/// is set from the [`crate::WidgetInfo`] the widget reports, so a link stays a
/// link and a button stays a button.
///
/// Each run holds the logical text of a stretch of one bidi level, with its direction.
/// Its character positions run from its left edge for left-to-right text and from
/// its right edge for right-to-left text, as AccessKit expects.
pub fn update_accesskit_for_text_widget(
    ctx: &Context,
    widget_id: Id,
    cursor_range: Option<CCursorRange>,
    global_from_galley: TSTransform,
    galley: &Galley,
) {
    let parent_id = ctx.accesskit_node_builder(widget_id, |builder| {
        let parent_id = widget_id;

        if let Some(cursor_range) = &cursor_range {
            let anchor = galley.layout_from_cursor(cursor_range.secondary);
            let focus = galley.layout_from_cursor(cursor_range.primary);
            builder.set_text_selection(accesskit::TextSelection {
                anchor: text_run_position(
                    parent_id,
                    galley,
                    anchor.row,
                    anchor.column,
                    cursor_range.secondary.prefer_next_row,
                ),
                focus: text_run_position(
                    parent_id,
                    galley,
                    focus.row,
                    focus.column,
                    cursor_range.primary.prefer_next_row,
                ),
            });
        }

        parent_id
    });

    let Some(parent_id) = parent_id else {
        return;
    };

    // The UI language is stamped on the AccessKit root and inherited by every
    // node (see `Context::begin_pass`); a text run whose content language is
    // known AND differs from it re-stamps the property here.
    let ui_language = ctx.language();

    let mut prev_row_ended_with_newline = true;

    for (row_index, row) in galley.rows.iter().enumerate() {
        // For soft-wrapped continuation rows, treat the start as a word
        // boundary so the first word character gets a `word_starts` entry.
        // Paragraph-starting runs (first row or after a newline) get an
        // implicit word start from AccessKit, so they don't need this.
        let mut was_at_word_end = !prev_row_ended_with_newline;
        let mut word_starts = Vec::<usize>::new();
        for (i, glyph) in row.glyphs.iter().enumerate() {
            let is_word_char = is_word_char(glyph.chr);
            if is_word_char && was_at_word_end {
                word_starts.push(i);
            }
            was_at_word_end = !is_word_char;
        }

        let runs = row_text_runs(row);
        let row_rect = row.rect_without_leading_space();

        for (run_index, run) in runs.iter().enumerate() {
            let glyphs = &row.glyphs[run.start..run.end.min(row.glyphs.len())];
            let is_rtl = glyphs.first().is_some_and(|glyph| glyph.is_rtl());

            // The run's horizontal extent, row-relative:
            let (x0, x1) = if glyphs.is_empty() {
                (row_rect.min.x - row.pos.x, row_rect.max.x - row.pos.x)
            } else {
                glyphs
                    .iter()
                    .fold((f32::INFINITY, f32::NEG_INFINITY), |(x0, x1), glyph| {
                        (x0.min(glyph.pos.x), x1.max(glyph.max_x()))
                    })
            };

            let mut value = String::with_capacity(run.len());
            let mut character_lengths = Vec::<u8>::with_capacity(run.len());
            let mut character_positions = Vec::<f32>::with_capacity(run.len());
            let mut character_widths = Vec::<f32>::with_capacity(run.len());
            for glyph in glyphs {
                let old_len = value.len();
                value.push(glyph.chr);
                character_lengths.push((value.len() - old_len) as _);
                character_positions.push(if is_rtl {
                    x1 - glyph.max_x()
                } else {
                    glyph.pos.x - x0
                });
                character_widths.push(glyph.advance_width);
            }
            if glyphs.len() < run.len() {
                // The newline, at the end of the run:
                value.push('\n');
                character_lengths.push(1);
                character_positions.push(x1 - x0);
                character_widths.push(0.0);
            }

            let run_id = text_run_id(parent_id, row_index, run_index);
            ctx.register_accesskit_parent(run_id, parent_id);

            ctx.accesskit_node_builder(run_id, |builder| {
                builder.set_role(accesskit::Role::TextRun);
                builder.set_text_direction(if is_rtl {
                    accesskit::TextDirection::RightToLeft
                } else {
                    accesskit::TextDirection::LeftToRight
                });
                if let Some(language) = &galley.job.language
                    && ui_language.as_deref() != Some(language.as_str())
                {
                    builder.set_language(language.clone());
                }
                // TODO(mwcampbell): Set more node fields for the row
                // once AccessKit adapters expose text formatting info.

                if run_index > 0 {
                    let prev_id = text_run_id(parent_id, row_index, run_index - 1);
                    builder.set_previous_on_line(prev_id.accesskit_id());
                }
                if run_index + 1 < runs.len() {
                    let next_id = text_run_id(parent_id, row_index, run_index + 1);
                    builder.set_next_on_line(next_id.accesskit_id());
                }

                let run_rect = emath::Rect::from_min_max(
                    emath::pos2(row.pos.x + x0, row_rect.min.y),
                    emath::pos2(row.pos.x + x1, row_rect.max.y),
                );
                let rect = global_from_galley * run_rect;
                builder.set_bounds(accesskit::Rect {
                    x0: rect.min.x.into(),
                    y0: rect.min.y.into(),
                    x1: rect.max.x.into(),
                    y1: rect.max.y.into(),
                });
                builder.set_value(value);
                builder.set_character_lengths(character_lengths);
                builder.set_character_positions(character_positions);
                builder.set_character_widths(character_widths);

                let run_word_starts: Vec<u8> = word_starts
                    .iter()
                    .filter(|&&ws| run.contains(&ws))
                    .map(|&ws| (ws - run.start) as u8)
                    .collect();
                builder.set_word_starts(run_word_starts);
            });
        }

        prev_row_ended_with_newline = row.ends_with_newline;
    }
}

#[cfg(test)]
mod tests {
    use accesskit::{Node, NodeId, Role, TextDirection, TreeUpdate};

    use super::MAX_CHARS_PER_TEXT_RUN;
    use epaint::text::cursor::CCursor;

    use super::{row_text_runs, text_run_position};
    use crate::{Color32, Context, FontId, Id, RawInput, Ui, UiBuilder};

    fn accesskit_update(ctx: &Context, mut add_contents: impl FnMut(&mut Ui)) -> TreeUpdate {
        ctx.enable_accesskit();
        let mut output = ctx.run_ui(RawInput::default(), |ui| add_contents(ui));
        output.textures_delta.clear();
        output
            .platform_output
            .accesskit_update
            .expect("Missing accesskit update")
    }

    fn node(update: &TreeUpdate, id: NodeId) -> &Node {
        &update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == id)
            .unwrap()
            .1
    }

    fn label<'a>(update: &'a TreeUpdate, text: &str) -> &'a Node {
        &update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Label && node.value() == Some(text))
            .unwrap_or_else(|| panic!("No label {text:?}"))
            .1
    }

    /// `(value, direction)` of each text run of a label, in order.
    fn runs(update: &TreeUpdate, text: &str) -> Vec<(String, TextDirection)> {
        label(update, text)
            .children()
            .iter()
            .map(|&id| node(update, id))
            .filter(|run| run.role() == Role::TextRun)
            .map(|run| {
                (
                    run.value().unwrap().to_owned(),
                    run.text_direction().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn ui_language_on_the_root_and_overrides_on_widgets() {
        let ctx = Context::default();
        ctx.set_language(Some("ar".to_owned()));
        let update = accesskit_update(&ctx, |ui| {
            assert_eq!(ui.language().as_deref(), Some("ar"));
            ui.label("مرحبا");
            ui.label("Bonjour").accessible_language("fr");
            ui.scope_builder(UiBuilder::new().id_salt("hebrew").language("he"), |ui| {
                assert_eq!(ui.language().as_deref(), Some("he"));
                ui.label("שלום");
            });
        });

        let root = node(&update, update.tree.as_ref().unwrap().root);
        assert_eq!(root.language(), Some("ar"));
        assert_eq!(
            label(&update, "مرحبا").language(),
            None,
            "inherits the root's"
        );
        assert_eq!(label(&update, "Bonjour").language(), Some("fr"));

        let hebrew = label(&update, "שלום").language();
        assert_eq!(hebrew, None, "inherits its Ui's");
        let hebrew_id = update
            .nodes
            .iter()
            .find(|(_, node)| node.value() == Some("שלום"))
            .unwrap()
            .0;
        let scope = &update
            .nodes
            .iter()
            .find(|(_, node)| node.children().contains(&hebrew_id))
            .unwrap()
            .1;
        assert_eq!(scope.language(), Some("he"));
    }

    #[test]
    fn text_runs_have_their_direction_and_logical_text() {
        let ctx = Context::default();
        let arabic = "مرحبا بالعالم";
        let mixed = "Hello שלום world";
        let update = accesskit_update(&ctx, |ui| {
            ui.label(arabic);
            ui.label(mixed);
        });

        assert_eq!(
            runs(&update, arabic),
            [(arabic.to_owned(), TextDirection::RightToLeft)]
        );
        assert_eq!(
            runs(&update, mixed),
            [
                ("Hello ".to_owned(), TextDirection::LeftToRight),
                ("שלום".to_owned(), TextDirection::RightToLeft),
                (" world".to_owned(), TextDirection::LeftToRight),
            ]
        );

        // Positions of a right-to-left run count from its right edge,
        // so they grow in logical order and stay inside the run.
        let run = node(&update, label(&update, arabic).children()[0]);
        let positions = run.character_positions().unwrap();
        let widths = run.character_widths().unwrap();
        let bounds = run.bounds().unwrap();
        assert_eq!(positions.len(), arabic.chars().count());
        assert!(positions[0].abs() < 0.01, "{positions:?}");
        assert!(positions.is_sorted(), "{positions:?}");
        let end = positions.last().unwrap() + widths.last().unwrap();
        assert!(
            (f64::from(end) - bounds.width()).abs() < 0.01,
            "{end} vs {}",
            bounds.width()
        );

        // Runs on one line link to each other in logical order:
        let ids = label(&update, mixed).children();
        assert_eq!(node(&update, ids[0]).next_on_line(), Some(ids[1]));
        assert_eq!(node(&update, ids[2]).previous_on_line(), Some(ids[1]));
    }

    #[test]
    fn text_positions_round_trip_through_runs() {
        let ctx = Context::default();
        let id = Id::unique("text");
        accesskit_update(&ctx, |ui| {
            let text = "abc אבג 12 def\nשלום";
            let galley = ui.painter().layout_no_wrap(
                text.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            assert!(row_text_runs(&galley.rows[0]).len() >= 3);
            for index in 0..=text.chars().count() {
                for prefer_next_row in [false, true] {
                    let cursor = CCursor {
                        index: epaint::text::CharIndex(index),
                        prefer_next_row,
                    };
                    let layout = galley.layout_from_cursor(cursor);
                    let position =
                        text_run_position(id, &galley, layout.row, layout.column, prefer_next_row);
                    let back =
                        crate::text_selection::cursor_range::ccursor_from_accesskit_text_position(
                            id, &galley, &position,
                        )
                        .unwrap();
                    assert_eq!(back.index, cursor.index, "{position:?}");
                    // Where the direction changes, the caret stays at the same run edge:
                    assert_eq!(
                        galley.pos_from_cursor(back),
                        galley.pos_from_cursor(cursor),
                        "{cursor:?} {position:?}"
                    );
                }
            }
        });
    }

    /// AccessKit's implicit first word start: `word_starts` lists the words of
    /// each run's value after the first, in logical order (the fork's runs are
    /// logical, so the run order here is the logical one).
    #[test]
    fn word_starts_follow_the_logical_text() {
        let ctx = Context::default();
        let mixed = "Hello שלום world";
        let update = accesskit_update(&ctx, |ui| {
            ui.label(mixed);
        });

        let ids = label(&update, mixed).children();
        assert_eq!(ids.len(), 3, "{:?}", runs(&update, mixed));

        // Logical text "Hello שלום world": words at logical chars 6 and 11
        // (the paragraph's first word start is implicit in AccessKit, see the
        // `was_at_word_end` seeding). Run 0 = logical [0..6) = "Hello " (no
        // word starts), run 1 = [6..10) = "שלום" (word at its local 0), run
        // 2 = [10..16) = " world" (the word starts after its leading space,
        // at local 1):
        let word_starts = |id: NodeId| node(&update, id).word_starts();
        assert_eq!(word_starts(ids[0]), &[][..]);
        assert_eq!(
            word_starts(ids[1]),
            &[0u8][..],
            "שלום starts its own run value"
        );
        assert_eq!(word_starts(ids[2]), &[1u8][..], "'world' follows the space");
    }

    /// An RTL-base row's runs carry their own directions and map logical
    /// columns back and forth exactly (what an AT-driven `SetTextSelection`
    /// uses). The fork's runs are logical, so the tree lists the Arabic run
    /// first.
    #[test]
    fn rtl_base_row_maps_logical_columns_through_the_right_runs() {
        let ctx = Context::default();
        let id = Id::unique("rtl_text");
        let arabic = "عربي 123";
        let mut galley_slot = None;
        let update = accesskit_update(&ctx, |ui| {
            let galley = ui.painter().layout_no_wrap(
                arabic.to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            assert!(
                galley.rows.first().is_some_and(|row| row.rtl_paragraph),
                "the label must resolve RTL"
            );
            assert_eq!(galley.rows.len(), 1);
            assert_eq!(row_text_runs(&galley.rows[0]).len(), 2, "digits and arabic split");
            galley_slot = Some(galley);
            ui.label(arabic);
        });

        // `(value, direction)` per run, in the tree's (logical) order:
        let run_values: Vec<(String, TextDirection)> = runs(&update, arabic);
        assert_eq!(
            run_values,
            vec![
                ("عربي ".to_owned(), TextDirection::RightToLeft),
                ("123".to_owned(), TextDirection::LeftToRight),
            ],
            "{run_values:?}"
        );

        // The runs joined in the tree's logical order read back the original
        // string:
        assert_eq!(
            format!("{}{}", run_values[0].0, run_values[1].0),
            arabic,
            "the logical run order reconstructs the text"
        );

        // Every logical column of the row maps to a run node + character
        // index, and round-trips back to the same column through the
        // reverse mapping:
        let galley = galley_slot.expect("galley");
        let n = arabic.chars().count();
        for column in 0..=n {
            for prefer_next_row in [false, true] {
                let cursor = CCursor {
                    index: epaint::text::CharIndex(column),
                    prefer_next_row,
                };
                let layout = galley.layout_from_cursor(cursor);
                let position =
                    text_run_position(id, &galley, layout.row, layout.column, prefer_next_row);
                let back = crate::text_selection::cursor_range::ccursor_from_accesskit_text_position(
                    id, &galley, &position,
                )
                .expect("position maps back");
                assert_eq!(back.index, cursor.index, "{cursor:?} -> {position:?}");
                // The caret stays put (a TextPosition cannot carry the
                // `prefer_next_row` flag; the reverse mapping re-derives it):
                assert_eq!(
                    galley.pos_from_cursor(back),
                    galley.pos_from_cursor(cursor),
                    "{cursor:?} {position:?}"
                );
            }
        }

        // Column 0 (logical start) is the START of the logically-first run
        // (the Arabic one, run index 0 in the fork's logical order); the row
        // end is the END of the digits run (index 1):
        let start = text_run_position(id, &galley, 0, epaint::text::CharIndex(0), false);
        assert_eq!(
            start.node,
            id.with(0usize).with(0usize).accesskit_id(),
            "column 0 must map to the logically-first (Arabic) run"
        );
        assert_eq!(start.character_index, 0);
        let end = text_run_position(id, &galley, 0, epaint::text::CharIndex(n), false);
        assert_eq!(end.node, id.with(0usize).with(1usize).accesskit_id());
        assert_eq!(end.character_index, 3);
    }

    /// A long single-run row splits into 255-char chunks (AccessKit's
    /// `word_starts` indices are `u8`), chained `next_on_line`/`previous_on_line`,
    /// with word starts as indices into each chunk's value.
    #[test]
    fn long_ltr_rows_chunk_every_255_chars() {
        let ctx = Context::default();
        let text = "ab ".repeat(100); // 300 chars, words at every multiple of 3
        let update = accesskit_update(&ctx, |ui| {
            ui.add(crate::Label::new(text.clone()).wrap_mode(crate::TextWrapMode::Extend));
        });

        let ids = label(&update, &text).children();
        assert_eq!(
            ids.len(),
            300usize.div_ceil(MAX_CHARS_PER_TEXT_RUN),
            "two 255-capped chunks"
        );
        let values: Vec<String> = ids
            .iter()
            .map(|&id| node(&update, id).value().expect("value").to_owned())
            .collect();
        assert_eq!(values.concat(), text);
        assert_eq!(values[0].chars().count(), MAX_CHARS_PER_TEXT_RUN);
        assert_eq!(node(&update, ids[0]).next_on_line(), Some(ids[1]));
        assert_eq!(node(&update, ids[1]).previous_on_line(), Some(ids[0]));
        // Word starts are indices into each chunk's value. The paragraph's
        // first word start is implicit in AccessKit (not pushed), so chunk 0
        // holds the words at 3, 6, …, 252 (84 of them); the word at logical
        // char 255 opens chunk 1 at its own char 0 (15 more to the end):
        let chunk0 = node(&update, ids[0]).word_starts();
        let chunk1 = node(&update, ids[1]).word_starts();
        assert_eq!(chunk0.len(), 84);
        assert_eq!(&chunk0[..3], &[3u8, 6, 9]);
        assert_eq!(chunk1.len(), 15);
    }

    /// The UI language is stamped on the AccessKit root (inherited by every
    /// node); a text run whose content language is known and different
    /// re-stamps it on its run nodes, content in the UI's language inherits.
    #[test]
    fn ui_language_on_the_root_and_content_language_on_text_runs() {
        let ctx = Context::default();
        ctx.set_language(Some("ar".to_owned()));

        // A LayoutJob carrying its own language (what per-Ui language plumbing
        // fills in; plain labels leave it unset):
        let update = accesskit_update(&ctx, |ui| {
            ui.label("مرحبا");
            let mut job = epaint::text::LayoutJob::simple(
                "Bonjour".to_owned(),
                FontId::proportional(14.0),
                Color32::WHITE,
                f32::INFINITY,
            );
            job.language = Some("fr".to_owned());
            ui.add(crate::Label::new(crate::WidgetText::from(job)));
        });

        let root = node(&update, update.tree.as_ref().expect("tree").root);
        assert_eq!(root.language(), Some("ar"));

        // Plain label: its job has no language → the run inherits the root's:
        let arabic = label(&update, "مرحبا");
        let run = node(&update, arabic.children()[0]);
        assert_eq!(run.role(), Role::TextRun);
        assert_eq!(
            run.language(),
            None,
            "the run must inherit the root's language"
        );

        // Content in another language re-stamps the property on the run:
        let french = label(&update, "Bonjour");
        let run = node(&update, french.children()[0]);
        assert_eq!(run.role(), Role::TextRun);
        assert_eq!(run.language(), Some("fr"));
    }
}
