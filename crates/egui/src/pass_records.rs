//! Records of one part of a pass ([`crate::Context::pass_records_since`]), which a later pass adds again in place of
//! running that part ([`crate::Context::replay_pass_records`]).

use crate::{Id, LayerId, WidgetRect};
use epaint::ClippedShape;

/// Where the current pass's shapes, widget rects and AccessKit nodes stand ([`crate::Context::pass_mark`]). It is
/// valid in the pass that made it.
#[derive(Clone, Debug, Default)]
pub struct PassMark {
    pub(crate) shapes: Vec<(LayerId, usize)>,
    pub(crate) widgets: Vec<(LayerId, usize)>,
    pub(crate) nodes: usize,
}

impl PassMark {
    /// How many records `layer` had in `marks` at the mark.
    pub(crate) fn at(marks: &[(LayerId, usize)], layer: LayerId) -> usize {
        marks
            .iter()
            .find(|(at, _)| *at == layer)
            .map_or(0, |(_, len)| *len)
    }
}

/// The shapes, widget rects and AccessKit nodes one part of a pass added, each as it stood at the end of that part.
#[derive(Clone, Default)]
pub struct PassRecords {
    pub(crate) shapes: Vec<(LayerId, Vec<ClippedShape>)>,
    pub(crate) widgets: Vec<WidgetRect>,
    pub(crate) nodes: Vec<(Id, accesskit::Node)>,

    /// The AccessKit parent of each widget and node that has one.
    pub(crate) parents: Vec<(Id, Id)>,
}

impl PassRecords {
    /// The layers the part painted on or placed widgets on.
    pub fn layers(&self) -> impl Iterator<Item = LayerId> + '_ {
        core::iter::chain(
            self.shapes.iter().map(|(layer, _)| *layer),
            self.widgets.iter().map(|widget| widget.layer_id),
        )
    }

    /// The widget rects, in the order they were added.
    pub fn widgets(&self) -> &[WidgetRect] {
        &self.widgets
    }
}
