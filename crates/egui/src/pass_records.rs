//! Records of one part of a pass ([`crate::Context::pass_records_since`]), which a later pass adds again in place of
//! running that part ([`crate::Context::replay_pass_records`]).

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
    /// AccessKit nodes, AccessKit parents; `None` when both hold the same records.
    ///
    /// It covers what the records hold: the shapes painted on each layer, the widget rects, the AccessKit nodes
    /// created and the parents registered for the part's widgets and nodes. It does not cover the platform output
    /// (cursor, IME, commands), area or sublayer registrations, layer transforms, or the parents of nodes created
    /// before the mark.
    ///
    /// Every record compares with `==`; floats compare exactly, since a part run again with the same inputs gives the
    /// same values. A paint callback cannot be compared (its callback is opaque, and a backend wraps every callback in
    /// one type): a callback on either side is [`PassRecordsDiff::Callback`].
    pub fn first_difference(&self, other: &Self) -> Option<PassRecordsDiff> {
        for records in [self, other] {
            for (layer, shapes) in &records.shapes {
                if let Some(index) = shapes.iter().position(|shape| has_callback(&shape.shape)) {
                    return Some(PassRecordsDiff::Callback {
                        layer: *layer,
                        index,
                    });
                }
            }
        }
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
            if let Some((index, left, right)) = first_unequal(left, right, |a, b| a == b) {
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
    /// The `index`th shape on `layer`, on either side, is or holds a paint callback, which cannot be compared.
    Callback { layer: LayerId, index: usize },

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

/// The variant, the index and the rects of each side: the records themselves stay on the value.
impl fmt::Display for PassRecordsDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Callback { layer, index } => {
                write!(f, "paint callback: shape {index} on {layer:?}")
            }
            Self::ShapeLayer { index, .. } => write!(f, "painted layer {index}"),
            Self::Shape {
                layer,
                index,
                left,
                right,
            } => {
                let rects = |shape: &Option<ClippedShape>| {
                    shape
                        .as_ref()
                        .map(|shape| (shape.shape.visual_bounding_rect(), shape.clip_rect))
                };
                write!(
                    f,
                    "shape {index} on {layer:?}: (bounds, clip) {:?} / {:?}",
                    rects(left),
                    rects(right)
                )
            }
            Self::Widget { index, left, right } => write!(
                f,
                "widget rect {index}: {:?} / {:?}",
                left.map(|widget| widget.rect),
                right.map(|widget| widget.rect)
            ),
            Self::Node { index, left, right } => {
                let bounds = |node: &Option<(Id, accesskit::Node)>| node.as_ref()?.1.bounds();
                write!(
                    f,
                    "AccessKit node {index}: bounds {:?} / {:?}",
                    bounds(left),
                    bounds(right)
                )
            }
            Self::Parent { index, .. } => write!(f, "AccessKit parent {index}"),
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

/// Whether `shape` is or holds a paint callback.
fn has_callback(shape: &Shape) -> bool {
    match shape {
        Shape::Callback(_) => true,
        Shape::Vec(shapes) => shapes.iter().any(has_callback),
        _ => false,
    }
}
