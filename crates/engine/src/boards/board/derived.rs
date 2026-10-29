use super::edit::set_font_size;
use super::items::{owner_of_text, shape_padding, sticky_font_size, text_of};
use super::{
    Affine, BTreeSet, BezPath, BoardItem, Connector, Document, End, NodeId, NodeKind, PathEl, Point, Rect, Routing, Shape, Side, Vec2, VectorNetwork,
    is_board,
};

// ---------------------------------------------------------------------------------------------
// Derived data (the normalize pass).

/// Before text is measured: a sticky's note takes the size its length calls for, and a resized
/// shape keeps its text inside its outline.
pub(crate) fn settle_text(doc: &mut Document, before: &Document, touched: &BTreeSet<NodeId>) {
    if !is_board(doc) {
        return;
    }
    for &id in touched {
        let Some(n) = doc.node(id) else { continue };
        if let Some(owner) =
            owner_of_text(doc, id).or(Some(id)).filter(|o| doc.node(*o).is_some_and(|k| matches!(k.board, Some(BoardItem::Sticky { .. }))))
            && let Some(t) = text_of(doc, owner)
            && let Some(NodeKind::Text(td)) = doc.node(t).map(|k| &k.kind)
        {
            let size = sticky_font_size(&td.characters);
            let off = td.style.font_size != size || td.runs.iter().any(|r| r.style.font_size != size);
            if off && let Some(NodeKind::Text(td)) = doc.node_mut(t).map(|k| &mut k.kind) {
                set_font_size(td, size);
            }
            continue;
        }
        if let Some(BoardItem::Shape { shape }) = n.board
            && before.node(id).is_none_or(|b| (b.width, b.height) != (n.width, n.height))
        {
            let pad = shape_padding(shape, n.width, n.height);
            if n.layout.as_ref().is_some_and(|l| l.padding != pad)
                && let Some(l) = doc.node_mut(id).and_then(|k| k.layout.as_mut())
            {
                l.padding = pad;
            }
        }
    }
}

/// After layout: connectors run between their ends again (an end whose layer is gone stays where
/// it was), one moved on its own takes its free ends along, and labels sit at their middles. Only
/// the connectors and labels an edit can have moved are looked at: touched themselves, or with a
/// touched (or gone) end, connector or layer around them.
pub(crate) fn settle_routes(doc: &mut Document, before: &Document, touched: &BTreeSet<NodeId>) {
    if !is_board(doc) || touched.is_empty() {
        return;
    }
    let hit = |doc: &Document, id: NodeId| touched.contains(&id) || doc.ancestors(id).iter().any(|a| touched.contains(a));
    let (mut connectors, mut labels) = (Vec::new(), Vec::new());
    for n in doc.nodes() {
        match &n.board {
            Some(BoardItem::Connector(c)) => {
                let ends = [c.from.node(), c.to.node()].into_iter().flatten();
                if hit(doc, n.id) || ends.into_iter().any(|e| !doc.contains(e) || hit(doc, e)) {
                    connectors.push((n.id, *c));
                }
            }
            Some(BoardItem::Label { connector }) => labels.push((n.id, *connector)),
            _ => {}
        }
    }
    let rerouted: BTreeSet<NodeId> = connectors.iter().map(|(id, _)| *id).collect();
    for (id, mut c) in connectors {
        let changed = detach_gone_ends(doc, id, &mut c) | carry_free_ends(doc, before, touched, id, &mut c);
        if changed && let Some(n) = doc.node_mut(id) {
            n.board = Some(BoardItem::Connector(c));
        }
        if let Some(pts) = route(doc, &c) {
            place_route(doc, id, &pts, c.routing);
        }
    }
    for (label, connector) in labels {
        if rerouted.contains(&connector) || touched.contains(&connector) || hit(doc, label) {
            place_label(doc, label, connector);
        }
    }
}

/// The ends of connector `id` (as `c`) whose layer is gone or on another page stay where they are
/// drawn, as points. Whether any did.
fn detach_gone_ends(doc: &Document, id: NodeId, c: &mut Connector) -> bool {
    let (page, ends) = (doc.page_of(id), ends_now(doc, id));
    let mut detached = false;
    for (end, at) in [(&mut c.from, ends.map(|e| e.0)), (&mut c.to, ends.map(|e| e.1))] {
        if let (End::Node { node, .. }, Some(p)) = (*end, at)
            && (!doc.contains(node) || doc.page_of(node) != page)
        {
            *end = End::Point { x: p.x, y: p.y };
            detached = true;
        }
    }
    detached
}

/// Connector `id` (as `c`) moved by itself (a drag, a nudge: its place changed but not what it
/// joins): its free ends move with it. Whether they did.
fn carry_free_ends(doc: &Document, before: &Document, touched: &BTreeSet<NodeId>, id: NodeId, c: &mut Connector) -> bool {
    let (Some(b), Some(n)) = (before.node(id), doc.node(id)) else { return false };
    if !touched.contains(&id) || b.transform == n.transform || b.board != n.board {
        return false;
    }
    let Some(expected) = route(doc, c).and_then(|pts| finite_box(&route_path(&pts, c.routing))).map(|b| b.origin()) else { return false };
    let d = (doc.world_transform(id) * Point::ZERO) - expected;
    if d.hypot() <= 1e-9 {
        return false;
    }
    let mut moved = false;
    for end in [&mut c.from, &mut c.to] {
        if let End::Point { x, y } = end {
            (*x, *y) = (*x + d.x, *y + d.y);
            moved = true;
        }
    }
    moved
}

/// Label `label` at the middle of connector `connector`; gone with it when it is gone.
fn place_label(doc: &mut Document, label: NodeId, connector: NodeId) {
    let Some(mid) = doc.node(connector).and_then(|_| route_mid(doc, connector)) else {
        let _ = doc.remove(label);
        return;
    };
    let Some(n) = doc.node(label) else { return };
    let pw = n.parent.map_or(Affine::IDENTITY, |p| doc.world_transform(p));
    let local = pw.inverse() * mid;
    let t = Affine::translate((local.x - n.width / 2.0, local.y - n.height / 2.0));
    if n.transform != t
        && let Some(n) = doc.node_mut(label)
    {
        n.transform = t;
    }
}

/// Connector `id`'s first and last points on the page as it is drawn now.
pub(super) fn ends_now(doc: &Document, id: NodeId) -> Option<(Point, Point)> {
    let NodeKind::Vector { network } = &doc.node(id)?.kind else { return None };
    let w = doc.world_transform(id);
    Some((w * network.vertices.first()?.point(), w * network.vertices.last()?.point()))
}

/// The middle of connector `id`'s route by length, on the page.
fn route_mid(doc: &Document, id: NodeId) -> Option<Point> {
    let NodeKind::Vector { network } = &doc.node(id)?.kind else { return None };
    let w = doc.world_transform(id);
    let mut pts: Vec<Point> = Vec::new();
    figcraft_geom::kurbo::flatten(network.stroke_path().iter(), 0.25, |el| {
        if let PathEl::MoveTo(p) | PathEl::LineTo(p) = el {
            pts.push(w * p);
        }
    });
    let total: f64 = pts.windows(2).map(|s| (s[1] - s[0]).hypot()).sum();
    let mut left = total / 2.0;
    for s in pts.windows(2) {
        let (a, b) = (s[0], s[1]);
        let l = (b - a).hypot();
        if l >= left && l > 0.0 {
            return Some(a.lerp(b, left / l));
        }
        left -= l;
    }
    pts.first().copied()
}

/// The path through a route's page points `pts` (see [`route`]): one curve for a curved
/// connector, straight runs otherwise.
fn route_path(pts: &[Point], routing: Routing) -> BezPath {
    let mut path = BezPath::new();
    match (routing, pts) {
        (Routing::Curved, [p0, c0, c1, p1]) => {
            path.move_to(*p0);
            path.curve_to(*c0, *c1, *p1);
        }
        _ => {
            for (i, p) in pts.iter().enumerate() {
                if i == 0 { path.move_to(*p) } else { path.line_to(*p) }
            }
        }
    }
    path
}

/// The box a path is drawn in, when it is finite.
fn finite_box(path: &BezPath) -> Option<Rect> {
    let b = path.bounding_box();
    (b.x0.is_finite() && b.y0.is_finite() && b.x1.is_finite() && b.y1.is_finite()).then_some(b)
}

/// Write the route `pts` (page points, see [`route`]) into connector `id`: its box, place and
/// path. Untouched when it already runs there, so a connector that didn't move isn't an edit.
pub(super) fn place_route(doc: &mut Document, id: NodeId, pts: &[Point], routing: Routing) {
    let Some(n) = doc.node(id) else { return };
    let pw = n.parent.map_or(Affine::IDENTITY, |p| doc.world_transform(p));
    let local = pw.inverse() * route_path(pts, routing);
    let Some(b) = finite_box(&local) else { return };
    let rel = Affine::translate((-b.x0, -b.y0)) * local;
    let network = match routing {
        Routing::Curved => VectorNetwork::from_path(&rel),
        _ => VectorNetwork::from_points(&rel.elements().iter().filter_map(PathEl::end_point).collect::<Vec<_>>(), false),
    };
    let t = Affine::translate((b.x0, b.y0));
    let same =
        n.transform == t && n.width == b.width() && n.height == b.height() && matches!(&n.kind, NodeKind::Vector { network: old } if *old == network);
    if same {
        return;
    }
    if let Some(n) = doc.node_mut(id) {
        n.kind = NodeKind::Vector { network };
        n.transform = t;
        (n.width, n.height) = (b.width(), b.height());
    }
}

/// How far an elbow connector going around the objects it joins keeps from them.
const ROOM: f64 = 24.0;

/// The page points connector `c` runs through: from the middle of the side of each attached layer
/// facing the other end (or the side it names), straight, in right angles, or (curved) its two
/// ends with the curve's two control points between them.
pub fn route(doc: &Document, c: &Connector) -> Option<Vec<Point>> {
    let bounds = |e: &End| e.node().and_then(|n| doc.world_bounds(n));
    let centre = |e: &End, b: Option<Rect>| match e {
        End::Point { x, y } => Some(Point::new(*x, *y)),
        End::Node { .. } => b.map(|b| b.center()),
    };
    let (fb, tb) = (bounds(&c.from), bounds(&c.to));
    let (fc, tc) = (centre(&c.from, fb)?, centre(&c.to, tb)?);
    let (p0, s0) = attach(&c.from, fb, tc)?;
    let (p1, s1) = attach(&c.to, tb, fc)?;
    let pts = match c.routing {
        Routing::Straight => tidy(vec![p0, p1]),
        Routing::Elbow => elbow(p0, s0, p1, s1, &[fb, tb].into_iter().flatten().collect::<Vec<_>>()),
        Routing::Curved => curve(p0, s0, p1, s1),
    };
    pts.iter().all(|p| p.x.is_finite() && p.y.is_finite()).then_some(pts)
}

/// `pts` without repeats or turns that don't turn (three in a line across or down).
fn tidy(pts: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(pts.len());
    for p in pts {
        if out.last() == Some(&p) {
            continue;
        }
        if let [.., a, b] = out.as_slice()
            && ((a.x == b.x && b.x == p.x) || (a.y == b.y && b.y == p.y))
        {
            out.pop();
        }
        out.push(p);
    }
    out
}

/// Where end `e` attaches (its point, or the middle of its layer's side facing `other`) and that
/// side (`None`: a free point).
fn attach(e: &End, b: Option<Rect>, other: Point) -> Option<(Point, Option<Side>)> {
    match e {
        End::Point { x, y } => Some((Point::new(*x, *y), None)),
        End::Node { side, .. } => {
            let b = b?;
            let side = match side {
                Side::Auto => {
                    let d = other - b.center();
                    // Compare against the box's proportions: a wide box takes the sides more.
                    if d.x.abs() * b.height().max(1e-9) >= d.y.abs() * b.width().max(1e-9) {
                        if d.x >= 0.0 { Side::Right } else { Side::Left }
                    } else if d.y >= 0.0 {
                        Side::Bottom
                    } else {
                        Side::Top
                    }
                }
                s => *s,
            };
            Some((side.point(b), Some(side)))
        }
    }
}

/// A curve from `p0` to `p1` leaving and arriving along the sides' normals (a free end pulls no
/// way): the ends and the two control points between them.
fn curve(p0: Point, s0: Option<Side>, p1: Point, s1: Option<Side>) -> Vec<Point> {
    let reach = (p1 - p0).hypot() / 2.0;
    let pull = |p: Point, s: Option<Side>| p + s.map_or(Vec2::ZERO, |s| s.normal() * reach);
    vec![p0, pull(p0, s0), pull(p1, s1), p1]
}

/// A right-angled route from `p0` to `p1`, leaving and arriving along the sides' normals (`s`; a
/// free end follows the other): the short way when it keeps out of the attached objects' `boxes`,
/// else the cheapest of a few ways around them (by length, and turns).
fn elbow(p0: Point, s0: Option<Side>, p1: Point, s1: Option<Side>, boxes: &[Rect]) -> Vec<Point> {
    let direct = tidy(short_elbow(p0, s0.map(Side::horizontal), p1, s1.map(Side::horizontal)));
    let Some(all) = boxes.iter().copied().reduce(|a, b| a.union(b)) else { return direct };
    if !crosses(&direct, boxes) {
        return direct;
    }
    // Out of each end by ROOM, then across on one of a few lines: halfway, or clear of the objects.
    let out = |p: Point, s: Option<Side>| p + s.map_or(Vec2::ZERO, |s| s.normal() * ROOM);
    let (a, b) = (out(p0, s0), out(p1, s1));
    let mut ways = vec![vec![p0, a, Point::new(b.x, a.y), b, p1], vec![p0, a, Point::new(a.x, b.y), b, p1]];
    for x in [(a.x + b.x) / 2.0, all.x0 - ROOM, all.x1 + ROOM] {
        ways.push(vec![p0, a, Point::new(x, a.y), Point::new(x, b.y), b, p1]);
    }
    for y in [(a.y + b.y) / 2.0, all.y0 - ROOM, all.y1 + ROOM] {
        ways.push(vec![p0, a, Point::new(a.x, y), Point::new(b.x, y), b, p1]);
    }
    let cost = |r: &Vec<Point>| r.windows(2).map(|s| (s[1] - s[0]).hypot()).sum::<f64>() + ROOM * r.len() as f64;
    ways.into_iter().map(tidy).filter(|r| !crosses(r, boxes)).min_by(|x, y| cost(x).total_cmp(&cost(y))).unwrap_or(direct)
}

/// Whether the right-angled route `pts` runs through the inside of any of `boxes` (along an edge,
/// or out of one, is outside).
fn crosses(pts: &[Point], boxes: &[Rect]) -> bool {
    const EDGE: f64 = 0.5;
    pts.windows(2).any(|s| {
        let (a, b) = (s[0], s[1]);
        boxes.iter().any(|r| a.x.min(b.x) < r.x1 - EDGE && a.x.max(b.x) > r.x0 + EDGE && a.y.min(b.y) < r.y1 - EDGE && a.y.max(b.y) > r.y0 + EDGE)
    })
}

/// The short right-angled route from `p0` to `p1` (`h`: the side faces sideways; a free end
/// follows the other): halfway across between two sides facing the same way, one turn otherwise.
fn short_elbow(p0: Point, h0: Option<bool>, p1: Point, h1: Option<bool>) -> Vec<Point> {
    let h0 = h0.or(h1).unwrap_or((p1.x - p0.x).abs() >= (p1.y - p0.y).abs());
    let h1 = h1.unwrap_or(h0);
    match (h0, h1) {
        (true, true) => {
            let mx = (p0.x + p1.x) / 2.0;
            vec![p0, Point::new(mx, p0.y), Point::new(mx, p1.y), p1]
        }
        (false, false) => {
            let my = (p0.y + p1.y) / 2.0;
            vec![p0, Point::new(p0.x, my), Point::new(p1.x, my), p1]
        }
        (true, false) => vec![p0, Point::new(p1.x, p0.y), p1],
        (false, true) => vec![p0, Point::new(p0.x, p1.y), p1],
    }
}
