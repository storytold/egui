//! Records of one part of a pass ([`crate::Context::pass_records_since`]), which a later pass adds again in place of
//! running that part ([`crate::Context::replay_pass_records`]).

use core::any::Any;
use core::fmt;

use crate::{Id, LayerId, WidgetRect};
use epaint::{ClippedShape, Shape};

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
        self.shapes
            .iter()
            .map(|(layer, _)| *layer)
            .chain(self.widgets.iter().map(|widget| widget.layer_id))
    }

    /// The widget rects, in the order they were added.
    pub fn widgets(&self) -> &[WidgetRect] {
        &self.widgets
    }

    /// The first record in which `self` and `other` differ, in the order shapes (layer by layer), widget rects,
    /// AccessKit nodes, AccessKit parents; `None` when both hold the same output.
    ///
    /// Every record compares with `==`, except a [`epaint::PaintCallback`]: it compares by its rect and the type of
    /// its callback, since each run makes a new callback (which `PartialEq` compares by pointer). Floats compare
    /// exactly: a part run again with the same inputs gives the same values.
    pub fn first_difference(&self, other: &Self) -> Option<PassRecordsDiff> {
        if let Some((index, left, right)) =
            first_unequal(&self.shapes, &other.shapes, |(a, _), (b, _)| a == b)
        {
            return Some(PassRecordsDiff::ShapeLayer {
                index,
                left: left.map(|(layer, _)| *layer),
                right: right.map(|(layer, _)| *layer),
            });
        }
        for ((layer, left), (_, right)) in self.shapes.iter().zip(&other.shapes) {
            if let Some((index, left, right)) = first_unequal(left, right, same_clipped_shape) {
                return Some(PassRecordsDiff::Shape {
                    layer: *layer,
                    index,
                    left: left.cloned(),
                    right: right.cloned(),
                });
            }
        }
        if let Some((index, left, right)) =
            first_unequal(&self.widgets, &other.widgets, |a, b| a == b)
        {
            return Some(PassRecordsDiff::Widget {
                index,
                left: left.copied(),
                right: right.copied(),
            });
        }
        if let Some((index, left, right)) = first_unequal(&self.nodes, &other.nodes, |a, b| a == b)
        {
            return Some(PassRecordsDiff::Node {
                index,
                left: left.cloned(),
                right: right.cloned(),
            });
        }
        if let Some((index, left, right)) =
            first_unequal(&self.parents, &other.parents, |a, b| a == b)
        {
            return Some(PassRecordsDiff::Parent {
                index,
                left: left.copied(),
                right: right.copied(),
            });
        }
        None
    }
}

/// The first difference between two [`PassRecords`] ([`PassRecords::first_difference`]): its kind, its index and the
/// record on each side, `None` on a side that has fewer records.
#[derive(Clone, Debug)]
pub enum PassRecordsDiff {
    /// The `index`th layer painted on.
    ShapeLayer {
        index: usize,
        left: Option<LayerId>,
        right: Option<LayerId>,
    },

    /// The `index`th shape painted on `layer`.
    Shape {
        layer: LayerId,
        index: usize,
        left: Option<ClippedShape>,
        right: Option<ClippedShape>,
    },

    /// The `index`th widget rect.
    Widget {
        index: usize,
        left: Option<WidgetRect>,
        right: Option<WidgetRect>,
    },

    /// The `index`th AccessKit node created.
    Node {
        index: usize,
        left: Option<(Id, accesskit::Node)>,
        right: Option<(Id, accesskit::Node)>,
    },

    /// The `index`th (child, AccessKit parent) pair.
    Parent {
        index: usize,
        left: Option<(Id, Id)>,
        right: Option<(Id, Id)>,
    },
}

impl fmt::Display for PassRecordsDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShapeLayer { index, left, right } => {
                write!(f, "painted layer {index}: {left:?} / {right:?}")
            }
            Self::Shape {
                layer,
                index,
                left,
                right,
            } => write!(f, "shape {index} on {layer:?}: {left:?} / {right:?}"),
            Self::Widget { index, left, right } => {
                write!(f, "widget rect {index}: {left:?} / {right:?}")
            }
            Self::Node { index, left, right } => {
                write!(f, "AccessKit node {index}: {left:?} / {right:?}")
            }
            Self::Parent { index, left, right } => {
                write!(f, "AccessKit parent {index}: {left:?} / {right:?}")
            }
        }
    }
}

/// The index of the first pair of `left` and `right` that `same` rejects, or where one list ends before the other,
/// with the record on each side.
fn first_unequal<'a, T>(
    left: &'a [T],
    right: &'a [T],
    same: impl Fn(&T, &T) -> bool,
) -> Option<(usize, Option<&'a T>, Option<&'a T>)> {
    let index = left
        .iter()
        .zip(right)
        .position(|(a, b)| !same(a, b))
        .or_else(|| (left.len() != right.len()).then(|| left.len().min(right.len())))?;
    Some((index, left.get(index), right.get(index)))
}

fn same_clipped_shape(a: &ClippedShape, b: &ClippedShape) -> bool {
    a.clip_rect == b.clip_rect && same_shape(&a.shape, &b.shape)
}

/// `Shape::eq`, except that a callback compares by its rect and its callback's type.
fn same_shape(a: &Shape, b: &Shape) -> bool {
    match (a, b) {
        (Shape::Vec(a), Shape::Vec(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_shape(a, b))
        }
        (Shape::Callback(a), Shape::Callback(b)) => {
            a.rect == b.rect && Any::type_id(&*a.callback) == Any::type_id(&*b.callback)
        }
        _ => a == b,
    }
}
