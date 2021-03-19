use super::*;

/// A node that adds padding to its child.
#[derive(Debug, Clone, PartialEq)]
pub struct PadNode {
    /// The amount of padding.
    pub padding: Sides<Linear>,
    /// The child node whose sides to pad.
    pub child: Node,
}

impl Layout for PadNode {
    fn layout(&self, ctx: &mut LayoutContext, areas: &Areas) -> Fragment {
        let areas = shrink(areas, self.padding);

        let mut layouted = self.child.layout(ctx, &areas);
        for frame in layouted.frames_mut() {
            pad(frame, self.padding);
        }

        layouted
    }
}

impl From<PadNode> for AnyNode {
    fn from(pad: PadNode) -> Self {
        Self::new(pad)
    }
}

/// Shrink all areas by the padding.
fn shrink(areas: &Areas, padding: Sides<Linear>) -> Areas {
    let shrink = |size| size - padding.resolve(size).size();
    Areas {
        current: shrink(areas.current),
        full: shrink(areas.full),
        backlog: areas.backlog.iter().copied().map(shrink).collect(),
        last: areas.last.map(shrink),
        expand: areas.expand,
    }
}

/// Enlarge the frame and move all elements inwards.
fn pad(frame: &mut Frame, padding: Sides<Linear>) {
    let padding = padding.resolve(frame.size);
    let origin = Point::new(padding.left, padding.top);

    frame.size += padding.size();
    for (point, _) in &mut frame.elements {
        *point += origin;
    }
}
