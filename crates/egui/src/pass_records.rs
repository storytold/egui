//! Records of one part of a pass ([`crate::Context::pass_records_since`]), which a later pass adds again in place of
//! running that part ([`crate::Context::replay_pass_records`]).

use core::fmt;

use crate::{Id, LayerId, WidgetRect};
use emath::{Pos2, Rect, Vec2};
use epaint::{ClippedShape, Shape};

/// Where the current pass's shapes, widget rects and AccessKit nodes stand ([`crate::Context::pass_mark`]). It is
/// valid in the pass that made it.
#[derive(Clone, Debug, Default)]
pub struct PassMark {
    pub(crate) shapes: Vec<(LayerId, usize)>,

    /// How many widget placements the pass had made ([`crate::WidgetRects::placed_since`]).
    pub(crate) widgets: u64,
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
    /// The records as a replay at the place `shift` names adds them ([`crate::Context::replay_pass_records`]).
    #[must_use]
    pub fn shifted(&self, shift: &RecordShift) -> Self {
        if shift.is_none() {
            return self.clone();
        }
        Self {
            shapes: self
                .shapes
                .iter()
                .map(|(layer, shapes)| {
                    (
                        *layer,
                        shapes.iter().map(|shape| shift.shape(shape)).collect(),
                    )
                })
                .collect(),
            widgets: self
                .widgets
                .iter()
                .map(|widget| shift.widget(widget))
                .collect(),
            nodes: self
                .nodes
                .iter()
                .map(|(id, node)| (*id, shift.node(node)))
                .collect(),
            parents: self.parents.clone(),
        }
    }

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

    /// Whether the records hold a paint callback. Its callback is opaque: it cannot be compared
    /// ([`Self::first_difference`]), and it may paint at the place it was made for, so a replay adds it there only
    /// ([`RecordShift`]).
    pub fn paints_callback(&self) -> bool {
        self.shapes
            .iter()
            .any(|(_, shapes)| shapes.iter().any(|shape| has_callback(&shape.shape)))
    }

    /// The first record in which `self` and `other` differ, in the order shapes (layer by layer), widget rects,
    /// AccessKit nodes, AccessKit parents; `None` when both hold the same records.
    ///
    /// It covers what the records hold: the shapes painted on each layer, the widget rects, the AccessKit nodes
    /// created and the parents registered for the part's widgets and nodes. It does not cover the platform output
    /// (cursor, IME, commands), area or sublayer registrations, layer transforms, or the parents of nodes created
    /// before the mark.
    ///
    /// Positions (points, rects, clip rects and AccessKit bounds) compare within `tolerance` points; everything else
    /// (ids, counts, colours, sizes, widths, angles, opacity, texture coordinates, text and its layout, flags) compares
    /// exactly. Records
    /// moved by [`Self::shifted`] from positions off the float grid differ from the part run at the new place by
    /// rounding only, which a small `tolerance` covers; `0.0` compares exactly. A paint callback cannot be compared
    /// (its callback is opaque, and a backend wraps every callback in one type): a callback on either side is
    /// [`PassRecordsDiff::Callback`].
    pub fn first_difference(&self, other: &Self, tolerance: f32) -> Option<PassRecordsDiff> {
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
        for ((layer, left), (_, right)) in core::iter::zip(&self.shapes, &other.shapes) {
            if let Some((index, left, right)) = first_unequal(left, right, |a, b| {
                near::clipped_shape(a, b, tolerance) == *b
            }) {
                return Some(PassRecordsDiff::Shape {
                    layer: *layer,
                    index,
                    left: left.cloned(),
                    right: right.cloned(),
                });
            }
        }
        if let Some((index, left, right)) = first_unequal(&self.widgets, &other.widgets, |a, b| {
            near::widget(a, b, tolerance) == *b
        }) {
            return Some(PassRecordsDiff::Widget {
                index,
                left: left.copied(),
                right: right.copied(),
            });
        }
        if let Some((index, left, right)) =
            first_unequal(&self.nodes, &other.nodes, |(a_id, a), (b_id, b)| {
                a_id == b_id && near::node(a, b, tolerance) == *b
            })
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

/// Where a replay adds records that a part made elsewhere ([`crate::Context::replay_pass_records`]).
///
/// The records move by `offset` in layer space, and show through `clip` where the part showed through `ran_clip`.
///
/// A clip rect equal to `ran_clip` is the clip of the code around the part, which becomes `clip`; any other clip rect
/// was set inside the part and moves with it, cut by `clip`. That gives what the part run at the new place gives when
/// `clip` is `ran_clip` moved by `offset`, or when the part lay wholly inside `ran_clip` (nothing of it was culled, and
/// every clip set inside it lies within `ran_clip`); the caller decides when it may move records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordShift {
    pub offset: Vec2,
    pub ran_clip: Rect,
    pub clip: Rect,
}

impl RecordShift {
    /// Records added where they were made, under `clip`.
    pub fn none(clip: Rect) -> Self {
        Self {
            offset: Vec2::ZERO,
            ran_clip: clip,
            clip,
        }
    }

    /// Whether the shift leaves every record as it is.
    pub fn is_none(&self) -> bool {
        self.offset == Vec2::ZERO && self.ran_clip == self.clip
    }

    /// `pos` moved with the records.
    pub fn pos(&self, pos: Pos2) -> Pos2 {
        pos + self.offset
    }

    /// `rect` moved with the records.
    pub fn rect(&self, rect: Rect) -> Rect {
        rect.translate(self.offset)
    }

    /// The clip rect `clip` of a record, at the new place.
    pub fn clip(&self, clip: Rect) -> Rect {
        if clip == self.ran_clip {
            self.clip
        } else {
            clip.translate(self.offset).intersect(self.clip)
        }
    }

    /// The shape `shape` recorded under `clip_rect`, at the new place.
    pub fn shape(&self, shape: &ClippedShape) -> ClippedShape {
        let mut moved = shape.clone();
        moved.clip_rect = self.clip(shape.clip_rect);
        if self.offset != Vec2::ZERO {
            moved.shape.translate(self.offset);
        }
        moved
    }

    /// The widget rect `widget` at the new place: its rect moved, the part of it that takes interaction cut by the new
    /// clip.
    pub fn widget(&self, widget: &WidgetRect) -> WidgetRect {
        WidgetRect {
            rect: self.rect(widget.rect),
            interact_rect: self.rect(widget.interact_rect).intersect(self.clip),
            ..*widget
        }
    }

    /// The AccessKit node `node` at the new place: its bounds moved. Bounds are in layer space for an untransformed
    /// layer only, so records on a transformed layer move by no offset.
    pub fn node(&self, node: &accesskit::Node) -> accesskit::Node {
        let mut moved = node.clone();
        if self.offset != Vec2::ZERO
            && let Some(bounds) = node.bounds()
        {
            let (x, y) = (f64::from(self.offset.x), f64::from(self.offset.y));
            moved.set_bounds(accesskit::Rect {
                x0: bounds.x0 + x,
                y0: bounds.y0 + y,
                x1: bounds.x1 + x,
                y1: bounds.y1 + y,
            });
        }
        moved
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
    let index = core::iter::zip(left, right)
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

/// Records with their geometry taken from another record where the two lie within a tolerance: a record that `==`
/// then finds equal to the other differs from it by no more than the tolerance in its positions and not at all
/// elsewhere. Each function takes the positions the matching [`RecordShift`] function moves: points, rects, clip rects
/// and AccessKit bounds. A shift moves no size, width, angle, opacity or texture coordinate, so those compare exactly.
mod near {
    use alloc::sync::Arc;

    use emath::{Pos2, Rangef, Rect};
    use epaint::{ClippedShape, Mesh, Shape};

    use crate::WidgetRect;

    fn float(a: f32, b: f32, tolerance: f32) -> f32 {
        if (a - b).abs() <= tolerance { b } else { a }
    }

    fn pos(a: Pos2, b: Pos2, tolerance: f32) -> Pos2 {
        Pos2::new(float(a.x, b.x, tolerance), float(a.y, b.y, tolerance))
    }

    fn rect(a: Rect, b: Rect, tolerance: f32) -> Rect {
        Rect::from_min_max(pos(a.min, b.min, tolerance), pos(a.max, b.max, tolerance))
    }

    fn points<const N: usize>(a: [Pos2; N], b: [Pos2; N], tolerance: f32) -> [Pos2; N] {
        core::array::from_fn(|index| pos(a[index], b[index], tolerance))
    }

    /// `a`, each item taken from `b`'s item by `near`, when both hold as many.
    fn list<T: Clone>(a: &[T], b: &[T], near: impl Fn(&T, &T) -> T) -> Vec<T> {
        if a.len() == b.len() {
            core::iter::zip(a, b).map(|(a, b)| near(a, b)).collect()
        } else {
            a.to_vec()
        }
    }

    pub(super) fn clipped_shape(
        a: &ClippedShape,
        b: &ClippedShape,
        tolerance: f32,
    ) -> ClippedShape {
        ClippedShape {
            clip_rect: rect(a.clip_rect, b.clip_rect, tolerance),
            shape: shape(&a.shape, &b.shape, tolerance),
            ..a.clone()
        }
    }

    fn shape(a: &Shape, b: &Shape, tolerance: f32) -> Shape {
        let t = tolerance;
        match (a, b) {
            (Shape::Vec(a), Shape::Vec(b)) => Shape::Vec(list(a, b, |a, b| shape(a, b, t))),
            (Shape::Circle(a), Shape::Circle(b)) => {
                let mut near = *a;
                near.center = pos(a.center, b.center, t);
                Shape::Circle(near)
            }
            (Shape::Ellipse(a), Shape::Ellipse(b)) => {
                let mut near = *a;
                near.center = pos(a.center, b.center, t);
                Shape::Ellipse(near)
            }
            (
                Shape::LineSegment {
                    points: a_points,
                    stroke,
                },
                Shape::LineSegment {
                    points: b_points, ..
                },
            ) => Shape::LineSegment {
                points: points(*a_points, *b_points, t),
                stroke: *stroke,
            },
            (Shape::Path(a), Shape::Path(b)) => {
                let mut near = a.clone();
                near.points = list(&a.points, &b.points, |a, b| pos(*a, *b, t));
                Shape::Path(near)
            }
            (Shape::Band(a), Shape::Band(b)) => {
                let mut near = a.clone();
                near.points = list(&a.points, &b.points, |a, b| {
                    let mut point = *a;
                    point.x = float(a.x, b.x, t);
                    point.y = Rangef::new(float(a.y.min, b.y.min, t), float(a.y.max, b.y.max, t));
                    point
                });
                Shape::Band(near)
            }
            (Shape::Rect(a), Shape::Rect(b)) => {
                let mut near = a.clone();
                near.rect = rect(a.rect, b.rect, t);
                Shape::Rect(near)
            }
            (Shape::Text(a), Shape::Text(b)) => {
                let mut near = a.clone();
                near.pos = pos(a.pos, b.pos, t);
                Shape::Text(near)
            }
            (Shape::Mesh(a), Shape::Mesh(b)) => Shape::Mesh(Arc::new(mesh(a, b, t))),
            (Shape::QuadraticBezier(a), Shape::QuadraticBezier(b)) => {
                let mut near = a.clone();
                near.points = points(a.points, b.points, t);
                Shape::QuadraticBezier(near)
            }
            (Shape::CubicBezier(a), Shape::CubicBezier(b)) => {
                let mut near = a.clone();
                near.points = points(a.points, b.points, t);
                Shape::CubicBezier(near)
            }
            _ => a.clone(),
        }
    }

    fn mesh(a: &Mesh, b: &Mesh, tolerance: f32) -> Mesh {
        Mesh {
            vertices: list(&a.vertices, &b.vertices, |a, b| {
                let mut vertex = *a;
                vertex.pos = pos(a.pos, b.pos, tolerance);
                vertex
            }),
            ..a.clone()
        }
    }

    pub(super) fn widget(a: &WidgetRect, b: &WidgetRect, tolerance: f32) -> WidgetRect {
        WidgetRect {
            rect: rect(a.rect, b.rect, tolerance),
            interact_rect: rect(a.interact_rect, b.interact_rect, tolerance),
            ..*a
        }
    }

    pub(super) fn node(
        a: &accesskit::Node,
        b: &accesskit::Node,
        tolerance: f32,
    ) -> accesskit::Node {
        let mut near = a.clone();
        if let (Some(a), Some(b)) = (a.bounds(), b.bounds()) {
            let tolerance = f64::from(tolerance);
            let float = |a: f64, b: f64| if (a - b).abs() <= tolerance { b } else { a };
            near.set_bounds(accesskit::Rect {
                x0: float(a.x0, b.x0),
                y0: float(a.y0, b.y0),
                x1: float(a.x1, b.x1),
                y1: float(a.y1, b.y1),
            });
        }
        near
    }
}
