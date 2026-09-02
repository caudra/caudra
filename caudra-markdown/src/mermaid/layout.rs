//! Layered graph layout on a character grid.
//!
//! A Sugiyama pipeline: break cycles, rank by longest path, insert waypoints
//! so no edge spans more than one rank, order each rank to reduce crossings,
//! then assign coordinates and route edges orthogonally.
//!
//! Everything runs in `along`/`across` space, where `along` is the rank axis
//! and `across` is the axis within a rank. Only the final mapping consults
//! the diagram direction, so one engine serves `TD`, `BT`, `LR` and `RL`.

use unicode_width::UnicodeWidthStr;

use super::parse::{Direction, EdgeStyle, Graph, Shape};

const LABEL_PAD: usize = 1;
const BORDER: usize = 2;
const NODE_GAP: usize = 2;
const LABEL_OFFSET: usize = 1;
const ORDER_PASSES: usize = 6;
const ALIGN_PASSES: usize = 4;
/// Blank columns kept either side of an edge label so two of them never read
/// as one word.
const LABEL_CLEARANCE: usize = 1;
/// The two rank-facing sides of a node, in the order ports are bucketed into.
const NEAR: usize = 0;
const FAR: usize = 1;
/// How far a label may be nudged across the flow before it is left where it
/// started. Past this it is further from its own edge than from someone
/// else's, which is worse than the overlap it is avoiding.
const LABEL_NUDGE_LIMIT: isize = 8;
/// A cluster border plus the blank ring that keeps it off its members.
const CLUSTER_PAD: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arrow {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedNode {
    pub node: usize,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedLabel {
    pub x: usize,
    pub y: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedEdge {
    /// Orthogonal polyline in screen cells. Consecutive points always share
    /// a row or a column, and the first and last land on a node border.
    pub points: Vec<(usize, usize)>,
    pub style: EdgeStyle,
    /// Head glyph direction, absent for open links.
    pub arrow: Option<(usize, usize, Arrow)>,
    pub label: Option<PlacedLabel>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedCluster {
    pub subgraph: usize,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub nodes: Vec<PlacedNode>,
    pub edges: Vec<PlacedEdge>,
    pub clusters: Vec<PlacedCluster>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Real(usize),
    Waypoint,
}

#[derive(Clone, Debug)]
struct Vertex {
    kind: Kind,
    rank: usize,
    across: usize,
    across_size: usize,
    along_size: usize,
    cluster: Option<usize>,
}

/// One rank-to-rank hop. Long edges become several, sharing an `edge` index.
#[derive(Clone, Copy, Debug)]
struct Hop {
    edge: usize,
    from: usize,
    to: usize,
}

#[derive(Clone, Debug, Default)]
struct Gap {
    /// Occupied across-intervals per channel, used for greedy colouring.
    rows: Vec<Vec<(usize, usize)>>,
    channel: Vec<(usize, usize)>,
    widest_label: usize,
}

/// The across interval a subgraph frame reserves at every rank it covers.
#[derive(Clone, Copy, Debug)]
struct Lane {
    cluster: usize,
    lo: usize,
    need: usize,
    first: usize,
    last: usize,
}

impl Lane {
    fn hi(&self) -> usize {
        self.lo + self.need
    }

    fn covers(&self, rank: usize) -> bool {
        self.first <= rank && rank <= self.last
    }

    fn blocks(&self, across: usize, size: usize) -> bool {
        across < self.hi() + CLUSTER_PAD && self.lo.saturating_sub(CLUSTER_PAD) < across + size
    }
}

#[derive(Clone, Copy, Debug)]
struct Band {
    start: usize,
    thickness: usize,
}

impl Band {
    fn gap_start(&self) -> usize {
        self.start + self.thickness
    }
}

#[derive(Clone, Copy, Debug)]
struct Route {
    hop: usize,
    from_across: usize,
    to_across: usize,
    start_along: usize,
    channel_along: usize,
    end_along: usize,
    arrow_along: Option<usize>,
    descending: bool,
}

pub fn layout(graph: &Graph) -> Layout {
    let mut builder = Builder::new(graph);
    builder.order_ranks();
    builder.assign_across();

    let ports = builder.assign_ports();
    let gaps = builder.allocate_channels(&ports);
    let bands = builder.band_extents(&gaps);
    let routes = builder.route(&ports, &bands, &gaps);
    builder.finish(&bands, &routes)
}

/// Across-interval a cluster's border occupies, in the same space as
/// `Vertex::across`, or `None` when it has no members on screen.
fn cluster_span(members: impl Iterator<Item = (usize, usize)>) -> Option<(usize, usize)> {
    let mut bounds: Option<(usize, usize)> = None;
    for (start, end) in members {
        bounds = Some(match bounds {
            None => (start, end),
            Some((low, high)) => (low.min(start), high.max(end)),
        });
    }
    bounds
}

struct Builder<'a> {
    graph: &'a Graph,
    vertical: bool,
    vertices: Vec<Vertex>,
    hops: Vec<Hop>,
    order: Vec<Vec<usize>>,
}

impl<'a> Builder<'a> {
    fn new(graph: &'a Graph) -> Self {
        let vertical = graph.direction.is_vertical();
        let ranks = rank_nodes(graph);
        let mut owner = vec![None; graph.nodes.len()];
        for (idx, subgraph) in graph.subgraphs.iter().enumerate() {
            for &node in &subgraph.nodes {
                owner[node] = Some(idx);
            }
        }
        let mut vertices: Vec<Vertex> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, node)| {
                let text = node
                    .label
                    .iter()
                    .map(|line| line.width())
                    .max()
                    .unwrap_or(0);
                let height = node.label.len() + BORDER;
                let width =
                    text + 2 * shape_padding(node.shape) + BORDER + node.shape.shear(height);
                Vertex {
                    kind: Kind::Real(idx),
                    rank: ranks[idx],
                    across: 0,
                    across_size: if vertical { width } else { height },
                    along_size: if vertical { height } else { width },
                    cluster: owner[idx],
                }
            })
            .collect();

        let mut hops = Vec::new();
        for (idx, edge) in graph.edges.iter().enumerate() {
            let (low, high) = (ranks[edge.from], ranks[edge.to]);
            if low.abs_diff(high) <= 1 {
                hops.push(Hop {
                    edge: idx,
                    from: edge.from,
                    to: edge.to,
                });
                continue;
            }
            // A waypoint joins the cluster only when the edge stays inside it,
            // so a link that leaves a subgraph is free to route around.
            let inside = match (owner[edge.from], owner[edge.to]) {
                (Some(from), Some(to)) if from == to => Some(from),
                _ => None,
            };
            let step: i64 = if high > low { 1 } else { -1 };
            let mut previous = edge.from;
            for offset in 1..low.abs_diff(high) {
                vertices.push(Vertex {
                    kind: Kind::Waypoint,
                    rank: (low as i64 + step * offset as i64) as usize,
                    across: 0,
                    across_size: 1,
                    along_size: 1,
                    cluster: inside,
                });
                let waypoint = vertices.len() - 1;
                hops.push(Hop {
                    edge: idx,
                    from: previous,
                    to: waypoint,
                });
                previous = waypoint;
            }
            hops.push(Hop {
                edge: idx,
                from: previous,
                to: edge.to,
            });
        }

        // A border cell can carry one edge. Without room for every edge on a
        // side they clamp onto the same cell, and a node with four inbound
        // edges shows one arrow instead of four.
        let mut demand = vec![[0usize; 2]; vertices.len()];
        for hop in &hops {
            let descending = vertices[hop.to].rank > vertices[hop.from].rank;
            let (leaves, arrives) = match descending {
                true => (FAR, NEAR),
                false => (NEAR, FAR),
            };
            demand[hop.from][leaves] += 1;
            demand[hop.to][arrives] += 1;
        }
        for (vertex, sides) in vertices.iter_mut().zip(&demand) {
            if matches!(vertex.kind, Kind::Real(_)) {
                vertex.across_size = vertex.across_size.max(sides[NEAR].max(sides[FAR]) + BORDER);
            }
        }

        let depth = vertices.iter().map(|v| v.rank).max().unwrap_or(0) + 1;
        let mut order = vec![Vec::new(); depth];
        for (idx, vertex) in vertices.iter().enumerate() {
            order[vertex.rank].push(idx);
        }

        Self {
            graph,
            vertical,
            vertices,
            hops,
            order,
        }
    }

    fn neighbours(&self, vertex: usize, previous_rank: bool) -> Vec<usize> {
        let rank = self.vertices[vertex].rank;
        let wanted = match previous_rank {
            true => rank.checked_sub(1),
            false => Some(rank + 1),
        };
        let Some(wanted) = wanted else {
            return Vec::new();
        };
        self.hops
            .iter()
            .filter_map(|hop| {
                let other = match (hop.from == vertex, hop.to == vertex) {
                    (true, _) => hop.to,
                    (_, true) => hop.from,
                    _ => return None,
                };
                (self.vertices[other].rank == wanted).then_some(other)
            })
            .collect()
    }

    /// Barycenter sweeps, keeping whichever ordering crossed least. Ties keep
    /// the incoming slot so the result is deterministic.
    fn order_ranks(&mut self) {
        let mut best = self.order.clone();
        let mut fewest = self.crossings(&best);

        for pass in 0..ORDER_PASSES {
            let downward = pass % 2 == 0;
            let ranks: Vec<usize> = match downward {
                true => (1..self.order.len()).collect(),
                false => (0..self.order.len().saturating_sub(1)).rev().collect(),
            };
            for rank in ranks {
                let positions = self.positions(&self.order);
                let mut scored: Vec<(usize, f64, usize)> = self.order[rank]
                    .iter()
                    .enumerate()
                    .map(|(slot, &vertex)| {
                        let peers = self.neighbours(vertex, downward);
                        let bary = match peers.is_empty() {
                            true => slot as f64,
                            false => {
                                peers.iter().map(|&p| positions[p] as f64).sum::<f64>()
                                    / peers.len() as f64
                            }
                        };
                        (vertex, bary, slot)
                    })
                    .collect();
                scored.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.2.cmp(&b.2)));
                self.order[rank] = scored.into_iter().map(|(vertex, _, _)| vertex).collect();
                self.group_clusters(rank);
            }
            let crossings = self.crossings(&self.order);
            if crossings < fewest {
                fewest = crossings;
                best = self.order.clone();
            }
        }
        self.order = best;
    }

    /// Members of one subgraph have to sit next to each other in every rank,
    /// otherwise no rectangle can enclose exactly that subgraph. Groups keep
    /// the barycenter order of their members and move as a unit.
    fn group_clusters(&mut self, rank: usize) {
        if self.graph.subgraphs.is_empty() {
            return;
        }
        let mut groups: Vec<(Option<usize>, Vec<usize>, usize)> = Vec::new();
        for (slot, &vertex) in self.order[rank].iter().enumerate() {
            let cluster = self.vertices[vertex].cluster;
            match groups.iter_mut().find(|(key, _, _)| *key == cluster) {
                Some((_, members, total)) => {
                    members.push(vertex);
                    *total += slot;
                }
                None => groups.push((cluster, vec![vertex], slot)),
            }
        }
        groups.sort_by(|a, b| (a.2 * b.1.len()).cmp(&(b.2 * a.1.len())));
        self.order[rank] = groups
            .into_iter()
            .flat_map(|(_, members, _)| members)
            .collect();
    }

    /// Clearance between neighbours in a rank, widened where a cluster border
    /// has to pass between them.
    fn separation(&self, left: usize, right: usize) -> usize {
        let (a, b) = (self.vertices[left].cluster, self.vertices[right].cluster);
        if a == b {
            return NODE_GAP;
        }
        NODE_GAP + usize::from(a.is_some()) * CLUSTER_PAD + usize::from(b.is_some()) * CLUSTER_PAD
    }

    fn positions(&self, order: &[Vec<usize>]) -> Vec<usize> {
        let mut positions = vec![0usize; self.vertices.len()];
        for rank in order {
            for (slot, &vertex) in rank.iter().enumerate() {
                positions[vertex] = slot;
            }
        }
        positions
    }

    fn crossings(&self, order: &[Vec<usize>]) -> usize {
        let positions = self.positions(order);
        let mut total = 0;
        for (index, first) in self.hops.iter().enumerate() {
            for second in &self.hops[index + 1..] {
                if self.vertices[first.from].rank != self.vertices[second.from].rank
                    || self.vertices[first.to].rank != self.vertices[second.to].rank
                {
                    continue;
                }
                let (a0, a1) = (positions[first.from], positions[first.to]);
                let (b0, b1) = (positions[second.from], positions[second.to]);
                if (a0 < b0 && a1 > b1) || (a0 > b0 && a1 < b1) {
                    total += 1;
                }
            }
        }
        total
    }

    /// Packs each rank, then pulls vertices toward the centre of their
    /// neighbours without letting them touch or swap.
    fn assign_across(&mut self) {
        for rank in &self.order.clone() {
            let mut cursor = match rank.first() {
                Some(&first) if self.vertices[first].cluster.is_some() => CLUSTER_PAD,
                _ => 0,
            };
            for (slot, &vertex) in rank.iter().enumerate() {
                self.vertices[vertex].across = cursor;
                cursor += self.vertices[vertex].across_size;
                if let Some(&next) = rank.get(slot + 1) {
                    cursor += self.separation(vertex, next);
                }
            }
        }

        for pass in 0..ALIGN_PASSES {
            let downward = pass % 2 == 0;
            let ranks: Vec<usize> = match downward {
                true => (0..self.order.len()).collect(),
                false => (0..self.order.len()).rev().collect(),
            };
            for rank in ranks {
                self.align_rank(rank, downward);
            }
        }

        self.reserve_cluster_lanes();

        // Normalise against the cluster borders too, so a subgraph flush with
        // the left edge still has room for its frame.
        let inset = self
            .vertices
            .iter()
            .map(|vertex| match vertex.cluster {
                Some(_) => vertex.across.saturating_sub(CLUSTER_PAD),
                None => vertex.across,
            })
            .min()
            .unwrap_or(0);
        for vertex in &mut self.vertices {
            vertex.across -= inset;
        }
    }

    /// A subgraph is one rectangle covering every rank it touches, so its
    /// across interval has to be the same at all of them. Packing each rank
    /// on its own lets a subgraph sit at one offset here and another there,
    /// and the rectangle spanning both then swallows whatever was between.
    fn reserve_cluster_lanes(&mut self) {
        let mut lanes: Vec<Lane> = (0..self.graph.subgraphs.len())
            .filter_map(|cluster| {
                let (first, last) = self.cluster_ranks(cluster)?;
                let lo = self
                    .vertices
                    .iter()
                    .filter(|vertex| vertex.cluster == Some(cluster))
                    .map(|vertex| vertex.across)
                    .min()?;
                let need = (first..=last)
                    .map(|rank| self.packed_extent(rank, Some(cluster)))
                    .max()
                    .unwrap_or(0);
                Some(Lane {
                    cluster,
                    lo,
                    need,
                    first,
                    last,
                })
            })
            .collect();
        if lanes.is_empty() {
            return;
        }
        lanes.sort_by_key(|lane| (lane.lo, lane.cluster));

        // Only frames sharing a rank can collide. Two that never overlap
        // along the flow are already apart, so they keep their packing.
        for idx in 0..lanes.len() {
            let floor = lanes[..idx]
                .iter()
                .filter(|other| other.first <= lanes[idx].last && lanes[idx].first <= other.last)
                .map(|other| other.hi() + CLUSTER_PAD * 2)
                .max()
                .unwrap_or(0);
            lanes[idx].lo = lanes[idx].lo.max(floor);
        }

        for rank in 0..self.order.len() {
            let members = self.order[rank].clone();
            for lane in lanes.iter().filter(|lane| lane.covers(rank)) {
                let mut cursor = lane.lo;
                for &vertex in &members {
                    if self.vertices[vertex].cluster != Some(lane.cluster) {
                        continue;
                    }
                    self.vertices[vertex].across = cursor;
                    cursor += self.vertices[vertex].across_size + NODE_GAP;
                }
            }

            // Outsiders keep the position alignment gave them unless a frame
            // is in the way, so a rank with no subgraph on it is untouched.
            let mut cursor = 0;
            for &vertex in &members {
                if self.vertices[vertex].cluster.is_some() {
                    continue;
                }
                let size = self.vertices[vertex].across_size;
                let mut across = self.vertices[vertex].across.max(cursor);
                // Each step clears one frame and strictly raises the target,
                // so this terminates.
                while let Some(lane) = lanes
                    .iter()
                    .find(|lane| lane.covers(rank) && lane.blocks(across, size))
                {
                    across = lane.hi() + CLUSTER_PAD;
                }
                self.vertices[vertex].across = across;
                cursor = across + size + NODE_GAP;
            }
        }
    }

    /// Across-extent the members of `cluster` need at `rank`, packed tight.
    fn packed_extent(&self, rank: usize, cluster: Option<usize>) -> usize {
        let sizes: Vec<usize> = self.order[rank]
            .iter()
            .filter(|&&vertex| self.vertices[vertex].cluster == cluster)
            .map(|&vertex| self.vertices[vertex].across_size)
            .collect();
        sizes.iter().sum::<usize>() + NODE_GAP * sizes.len().saturating_sub(1)
    }

    fn cluster_ranks(&self, cluster: usize) -> Option<(usize, usize)> {
        cluster_span(
            self.vertices
                .iter()
                .filter(|vertex| vertex.cluster == Some(cluster))
                .map(|vertex| (vertex.rank, vertex.rank)),
        )
    }

    /// Extra along-space the gap after `rank` needs for cluster frames. A
    /// cluster closing there and another opening on the next rank need one
    /// frame each, so the two cases are counted apart.
    fn cluster_along_pad(&self, rank: usize) -> usize {
        let spans: Vec<(usize, usize)> = (0..self.graph.subgraphs.len())
            .filter_map(|cluster| self.cluster_ranks(cluster))
            .collect();
        let opening = spans.iter().any(|&(first, _)| first == rank + 1);
        self.channel_base(rank) + usize::from(opening) * CLUSTER_PAD
    }

    /// Offset into the gap after `rank` where channels may start. A subgraph
    /// ending here closes its frame inside that gap, and an edge sharing the
    /// frame's column would draw itself on top of the border.
    fn channel_base(&self, rank: usize) -> usize {
        let closing = (0..self.graph.subgraphs.len())
            .filter_map(|cluster| self.cluster_ranks(cluster))
            .any(|(_, last)| last == rank);
        usize::from(closing) * CLUSTER_PAD
    }

    fn align_rank(&mut self, rank: usize, downward: bool) {
        let members = self.order[rank].clone();
        let desired: Vec<Option<usize>> = members
            .iter()
            .map(|&vertex| {
                let peers = self.neighbours(vertex, downward);
                if peers.is_empty() {
                    return None;
                }
                let sum: usize = peers.iter().map(|&peer| self.centre(peer)).sum();
                let centre = sum / peers.len();
                Some(centre.saturating_sub(self.vertices[vertex].across_size / 2))
            })
            .collect();

        for (slot, &vertex) in members.iter().enumerate() {
            let Some(target) = desired[slot] else {
                continue;
            };
            if target <= self.vertices[vertex].across {
                continue;
            }
            let ceiling = members.get(slot + 1).map(|&next| {
                let clearance = self.vertices[vertex].across_size + self.separation(vertex, next);
                self.vertices[next].across.saturating_sub(clearance)
            });
            let capped = match ceiling {
                Some(cap) => target.min(cap),
                None => target,
            };
            self.vertices[vertex].across = capped.max(self.vertices[vertex].across);
        }

        for (slot, &vertex) in members.iter().enumerate().rev() {
            let Some(target) = desired[slot] else {
                continue;
            };
            if target >= self.vertices[vertex].across {
                continue;
            }
            let floor = match slot.checked_sub(1) {
                // The first vertex still owes its cluster room for a frame.
                None => usize::from(self.vertices[vertex].cluster.is_some()) * CLUSTER_PAD,
                Some(before) => {
                    let previous = members[before];
                    self.vertices[previous].across
                        + self.vertices[previous].across_size
                        + self.separation(previous, vertex)
                }
            };
            self.vertices[vertex].across = target.max(floor);
        }
    }

    fn centre(&self, vertex: usize) -> usize {
        self.vertices[vertex].across + self.vertices[vertex].across_size / 2
    }

    /// Where each hop leaves its source and meets its target. Ports are
    /// grouped by the side of the node they use, not by edge direction, so a
    /// back edge arriving at a node cannot land on the same cell as an edge
    /// leaving it.
    /// Across-interval of a vertex that ports may sit on, relative to its own
    /// across origin. A slant runs along the across axis only when the ranks
    /// stack vertically, and there it narrows both horizontal faces.
    fn port_window(&self, vertex: usize) -> (usize, usize) {
        let vertex = &self.vertices[vertex];
        let whole = (0, vertex.across_size.saturating_sub(1));
        let Kind::Real(node) = vertex.kind else {
            return whole;
        };
        match self.vertical {
            true => self.graph.nodes[node]
                .shape
                .rule_window(vertex.across_size, vertex.along_size),
            false => whole,
        }
    }

    fn assign_ports(&self) -> Vec<(usize, usize)> {
        let mut ports = vec![(0usize, 0usize); self.hops.len()];
        let mut sides: Vec<[Vec<(usize, bool)>; 2]> =
            vec![[Vec::new(), Vec::new()]; self.vertices.len()];
        for (idx, hop) in self.hops.iter().enumerate() {
            let descending = self.vertices[hop.to].rank > self.vertices[hop.from].rank;
            let (leaves, arrives) = match descending {
                true => (FAR, NEAR),
                false => (NEAR, FAR),
            };
            sides[hop.from][leaves].push((idx, true));
            sides[hop.to][arrives].push((idx, false));
        }

        for (vertex, attached) in sides.into_iter().enumerate() {
            let (low, high) = self.port_window(vertex);
            let span = high - low + 1;
            let base = self.vertices[vertex].across + low;
            for mut side in attached {
                if side.is_empty() {
                    continue;
                }
                side.sort_by_key(|&(idx, is_source)| {
                    let hop = self.hops[idx];
                    self.centre(if is_source { hop.to } else { hop.from })
                });
                let count = side.len();
                for (slot, (idx, is_source)) in side.into_iter().enumerate() {
                    let offset = match count {
                        1 => span / 2,
                        _ => span * (slot + 1) / (count + 1),
                    };
                    let port = base + offset.clamp(1.min(span - 1), span.saturating_sub(2).max(1));
                    match is_source {
                        true => ports[idx].0 = port,
                        false => ports[idx].1 = port,
                    }
                }
            }
        }
        ports
    }

    fn edge_label(&self, hop: usize) -> Option<&'a str> {
        let target = self.hops[hop].edge;
        let label = self.graph.edges[target].label.as_deref()?;
        let first = self.hops.iter().position(|other| other.edge == target);
        (first == Some(hop)).then_some(label)
    }

    /// Greedy interval colouring per gap: a hop that travels sideways takes
    /// the lowest channel whose across-span is still free.
    fn allocate_channels(&self, ports: &[(usize, usize)]) -> Vec<Gap> {
        let mut gaps = vec![Gap::default(); self.order.len().saturating_sub(1)];
        for (idx, hop) in self.hops.iter().enumerate() {
            let low = self.vertices[hop.from].rank.min(self.vertices[hop.to].rank);
            let Some(gap) = gaps.get_mut(low) else {
                continue;
            };
            let (from, to) = ports[idx];
            let label = self.edge_label(idx).map(str::width).unwrap_or(0);
            gap.widest_label = gap.widest_label.max(label);
            if from == to && (label == 0 || !self.vertical) {
                continue;
            }
            // A vertical layout writes the label along the channel, so it
            // extends the reserved span. A horizontal one writes it on the
            // line above, which costs an extra across cell instead.
            let (start, end) = match self.vertical {
                true => (
                    from.min(to),
                    from.max(to) + if label > 0 { label + LABEL_OFFSET } else { 0 },
                ),
                false => (
                    from.min(to).saturating_sub(usize::from(label > 0)),
                    from.max(to),
                ),
            };
            let row = gap
                .rows
                .iter()
                .position(|taken| taken.iter().all(|&(a, b)| end < a || start > b))
                .unwrap_or_else(|| {
                    gap.rows.push(Vec::new());
                    gap.rows.len() - 1
                });
            gap.rows[row].push((start, end));
            gap.channel.push((idx, row));
        }
        gaps
    }

    /// Along-extent of every rank plus the gap that follows it. The gap keeps
    /// one clear cell for the arrowhead beyond the last channel.
    fn band_extents(&self, gaps: &[Gap]) -> Vec<Band> {
        let mut bands = Vec::with_capacity(self.order.len());
        let starts_clustered = (0..self.graph.subgraphs.len()).any(|cluster| {
            self.cluster_ranks(cluster)
                .is_some_and(|(first, _)| first == 0)
        });
        let mut cursor = usize::from(starts_clustered) * CLUSTER_PAD;
        for (rank, members) in self.order.iter().enumerate() {
            let thickness = members
                .iter()
                .map(|&v| self.vertices[v].along_size)
                .max()
                .unwrap_or(1)
                .max(1);
            let gap = gaps.get(rank).map_or(0, |gap| {
                let channels = gap.rows.len().max(1) + 1;
                let room = match self.vertical {
                    true => channels,
                    false => channels.max(gap.widest_label + LABEL_OFFSET + 1),
                };
                room + self.cluster_along_pad(rank)
            });
            bands.push(Band {
                start: cursor,
                thickness,
            });
            cursor += thickness + gap;
        }
        bands
    }

    /// A vertex owns only its own thickness, never the rank's. Taking the
    /// band's far edge instead detaches an edge from any node narrower than
    /// its widest neighbour, and makes the two legs of a long edge disagree
    /// about where their shared waypoint sits.
    fn along_span(&self, vertex: usize, bands: &[Band]) -> (usize, usize) {
        let start = bands[self.vertices[vertex].rank].start;
        (start, start + self.vertices[vertex].along_size - 1)
    }

    fn route(&self, ports: &[(usize, usize)], bands: &[Band], gaps: &[Gap]) -> Vec<Route> {
        let mut routes = Vec::with_capacity(self.hops.len());
        for (idx, hop) in self.hops.iter().enumerate() {
            let (source, target) = (&self.vertices[hop.from], &self.vertices[hop.to]);
            let descending = target.rank > source.rank;
            let low = source.rank.min(target.rank);
            let near = &bands[low];
            let channel = gaps[low]
                .channel
                .iter()
                .find(|(candidate, _)| *candidate == idx)
                .map_or(0, |&(_, row)| row);

            let (source_lo, source_hi) = self.along_span(hop.from, bands);
            let (target_lo, target_hi) = self.along_span(hop.to, bands);
            let target_real = matches!(target.kind, Kind::Real(_));
            let last_hop = self.hops[idx].edge;
            let arrowed = self.graph.edges[last_hop].arrow && target_real;

            // An arrowhead is the terminus: the polyline stops one cell short
            // of the target so the head sits against an unbroken border.
            let (start_along, border_along, arrow_along) = match descending {
                true => (source_hi, target_lo, arrowed.then(|| target_lo - 1)),
                false => (source_lo, target_hi, arrowed.then(|| target_hi + 1)),
            };
            let end_along = arrow_along.unwrap_or(border_along);

            routes.push(Route {
                hop: idx,
                from_across: ports[idx].0,
                to_across: ports[idx].1,
                start_along,
                channel_along: near.gap_start() + self.channel_base(low) + channel,
                end_along,
                arrow_along,
                descending,
            });
        }
        routes
    }

    fn finish(&self, bands: &[Band], routes: &[Route]) -> Layout {
        let along_total = bands
            .last()
            .map_or(1, |band| band.start + band.thickness)
            .max(1);
        let mapper = Mapper {
            direction: self.graph.direction,
            along_total,
        };

        let mut nodes: Vec<PlacedNode> = self
            .vertices
            .iter()
            .filter_map(|vertex| {
                let Kind::Real(node) = vertex.kind else {
                    return None;
                };
                let along = bands[vertex.rank].start;
                let (x, y) = mapper.rect(along, vertex.across, vertex.along_size);
                let (width, height) = match self.vertical {
                    true => (vertex.across_size, vertex.along_size),
                    false => (vertex.along_size, vertex.across_size),
                };
                Some(PlacedNode {
                    node,
                    x,
                    y,
                    width,
                    height,
                })
            })
            .collect();
        nodes.sort_by_key(|placed| placed.node);

        let mut edges = self.assemble(routes, &mapper);
        seat_ends(&mut edges, &nodes, self.graph);
        place_labels(&mut edges, &nodes, self.vertical);
        let clusters = self.enclose(bands, &mapper);
        let reach = |axis: fn(usize, usize) -> usize| {
            clusters
                .iter()
                .map(|cluster| axis(cluster.x + cluster.width, cluster.y + cluster.height))
                .max()
                .unwrap_or(0)
        };
        let width =
            extent(&nodes, &edges, |placed| placed.x + placed.width, |x, _| x).max(reach(|x, _| x));
        let height = extent(&nodes, &edges, |placed| placed.y + placed.height, |_, y| y)
            .max(reach(|_, y| y));
        Layout {
            width,
            height,
            nodes,
            edges,
            clusters,
        }
    }

    /// One rectangle per subgraph, inset by the pad reserved during packing.
    fn enclose(&self, bands: &[Band], mapper: &Mapper) -> Vec<PlacedCluster> {
        (0..self.graph.subgraphs.len())
            .filter_map(|cluster| {
                let members = || {
                    self.vertices
                        .iter()
                        .filter(move |vertex| vertex.cluster == Some(cluster))
                };
                let (low, high) = cluster_span(
                    members().map(|vertex| (vertex.across, vertex.across + vertex.across_size)),
                )?;
                let (first, last) = self.cluster_ranks(cluster)?;
                let across = (low.saturating_sub(CLUSTER_PAD), high + CLUSTER_PAD);
                let along = (
                    bands[first].start.saturating_sub(CLUSTER_PAD),
                    bands[last].start + bands[last].thickness + CLUSTER_PAD,
                );
                let (x, y) = mapper.rect(along.0, across.0, along.1 - along.0);
                let (span_along, span_across) = (along.1 - along.0, across.1 - across.0);
                let (width, height) = match self.vertical {
                    true => (span_across, span_along),
                    false => (span_along, span_across),
                };
                Some(PlacedCluster {
                    subgraph: cluster,
                    x,
                    y,
                    width,
                    height,
                })
            })
            .collect()
    }

    /// Stitches per-rank hops back into one polyline for each original edge.
    fn assemble(&self, routes: &[Route], mapper: &Mapper) -> Vec<PlacedEdge> {
        let mut edges = Vec::new();
        for index in 0..self.graph.edges.len() {
            let legs: Vec<&Route> = routes
                .iter()
                .filter(|route| self.hops[route.hop].edge == index)
                .collect();
            let Some(last) = legs.last() else { continue };

            let mut points: Vec<(usize, usize)> = Vec::new();
            let mut label = None;
            for leg in &legs {
                let corners = [
                    (leg.start_along, leg.from_across),
                    (leg.channel_along, leg.from_across),
                    (leg.channel_along, leg.to_across),
                    (leg.end_along, leg.to_across),
                ];
                for (along, across) in corners {
                    let point = mapper.point(along, across);
                    if points.last() != Some(&point) {
                        points.push(point);
                    }
                }
                if let Some(text) = self.edge_label(leg.hop) {
                    let (along, across) = match self.vertical {
                        true => (
                            leg.channel_along,
                            leg.from_across.max(leg.to_across) + LABEL_OFFSET,
                        ),
                        false => (
                            leg.channel_along + LABEL_OFFSET,
                            leg.from_across.min(leg.to_across).saturating_sub(1),
                        ),
                    };
                    let (x, y) = mapper.point(along, across);
                    label = Some(PlacedLabel {
                        x,
                        y,
                        text: text.to_owned(),
                    });
                }
            }

            let arrow = last.arrow_along.map(|along| {
                let (x, y) = mapper.point(along, last.to_across);
                (x, y, mapper.arrow(last.descending))
            });
            edges.push(PlacedEdge {
                points,
                style: self.graph.edges[index].style,
                arrow,
                label,
            });
        }
        edges
    }
}

/// Half-open rectangle used only to keep labels off each other and off the
/// boxes.
struct Occupied {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Occupied {
    fn hits(&self, x: usize, y: usize, width: usize) -> bool {
        x < self.x + self.width && self.x < x + width && y < self.y + self.height && self.y <= y
    }
}

/// Routing meets a node at its bounding box, which is where the border is for
/// every upright shape. A slant stands at a different column on every row, so
/// an end that arrives sideways has to walk in until it reaches one.
fn seat_ends(edges: &mut [PlacedEdge], nodes: &[PlacedNode], graph: &Graph) {
    for edge in edges {
        // The head sits one cell short of the border it points at, so the node
        // it belongs to starts one cell further on than the polyline does.
        let clearance = usize::from(edge.arrow.is_some());
        for (end, inset) in [(0, 0), (edge.points.len().saturating_sub(1), clearance)] {
            let Some(seated) = seat_end(&edge.points, end, inset, nodes, graph) else {
                continue;
            };
            edge.points[end] = seated;
            if end > 0 && edge.arrow.is_some() {
                edge.arrow = edge.arrow.map(|(_, _, arrow)| (seated.0, seated.1, arrow));
            }
        }
    }
}

/// Where `end` of a polyline belongs once the slant of the node it meets is
/// taken into account, or `None` when nothing needs to move.
fn seat_end(
    points: &[(usize, usize)],
    end: usize,
    inset: usize,
    nodes: &[PlacedNode],
    graph: &Graph,
) -> Option<(usize, usize)> {
    let (x, y) = *points.get(end)?;
    let neighbour = points.get(if end == 0 { 1 } else { end - 1 })?;
    if neighbour.1 != y || neighbour.0 == x {
        return None;
    }
    // Both ends measure travel toward the node: the start sits on the one it
    // leaves, the finish points at the one it meets.
    let step: isize = if neighbour.0 > x { -1 } else { 1 };
    let cell = x.checked_add_signed(step * inset as isize)?;
    let placed = nodes.iter().find(|placed| {
        (placed.x..placed.x + placed.width).contains(&cell)
            && (placed.y..placed.y + placed.height).contains(&y)
    })?;
    let shape = graph.nodes[placed.node].shape;
    shape.lean()?;
    let (left, right) = shape.sides(placed.width, placed.height, y - placed.y);
    let border = placed.x + if step > 0 { left } else { right };
    // A line merges into an upright border and takes a corner glyph there. A
    // slant has no junction to offer, so the line halts just short of it.
    let clear = inset.max(1) as isize;
    Some((border.checked_add_signed(-step * clear)?, y))
}

/// Edge labels are the only text that can land on top of other text, so they
/// are positioned after everything else and nudged across the flow until they
/// sit clear. Routing decides where a label wants to go, which is why two
/// edges leaving one decision can both ask for the same cell.
fn place_labels(edges: &mut [PlacedEdge], nodes: &[PlacedNode], vertical: bool) {
    let mut taken: Vec<Occupied> = nodes
        .iter()
        .map(|node| Occupied {
            x: node.x,
            y: node.y,
            width: node.width,
            height: node.height,
        })
        .collect();

    for edge in edges.iter_mut() {
        let Some(label) = edge.label.as_mut() else {
            continue;
        };
        // The clearance is carried in the test rectangle rather than in the
        // label's own position, so a label at the canvas edge is not pushed
        // off it.
        let left = label.x.saturating_sub(LABEL_CLEARANCE);
        let width = label.text.width() + LABEL_CLEARANCE + (label.x - left);

        let mut spot = None;
        for step in (0..=LABEL_NUDGE_LIMIT).flat_map(|step| [step, -step]) {
            // A vertical chart writes labels beside the channel and a
            // horizontal one above it, so each nudges along its free axis.
            let (x, y) = match vertical {
                true => (left.checked_add_signed(step), Some(label.y)),
                false => (Some(left), label.y.checked_add_signed(step)),
            };
            let (Some(x), Some(y)) = (x, y) else {
                continue;
            };
            if !taken.iter().any(|rect| rect.hits(x, y, width)) {
                spot = Some((x, y));
                break;
            }
        }

        let (x, y) = spot.unwrap_or((left, label.y));
        label.x = x + (label.x - left);
        label.y = y;
        taken.push(Occupied {
            x,
            y,
            width,
            height: 1,
        });
    }
}

fn extent(
    nodes: &[PlacedNode],
    edges: &[PlacedEdge],
    from_node: impl Fn(&PlacedNode) -> usize,
    axis: impl Fn(usize, usize) -> usize,
) -> usize {
    let boxes = nodes.iter().map(from_node).max().unwrap_or(0);
    let points = edges
        .iter()
        .flat_map(|edge| edge.points.iter())
        .map(|&(x, y)| axis(x, y) + 1)
        .max()
        .unwrap_or(0);
    let labels = edges
        .iter()
        .filter_map(|edge| edge.label.as_ref())
        .map(|label| axis(label.x + label.text.width(), label.y + 1))
        .max()
        .unwrap_or(0);
    boxes.max(points).max(labels).max(1)
}

/// Longest-path ranking over the acyclic view of the graph.
fn rank_nodes(graph: &Graph) -> Vec<usize> {
    let count = graph.nodes.len();
    let forward = forward_edges(graph);
    let mut incoming = vec![0usize; count];
    for &(_, to) in &forward {
        incoming[to] += 1;
    }

    let mut rank = vec![0usize; count];
    let mut ready: Vec<usize> = (0..count).filter(|&node| incoming[node] == 0).collect();
    while let Some(node) = ready.pop() {
        for &(from, to) in forward.iter().filter(|&&(from, _)| from == node) {
            rank[to] = rank[to].max(rank[from] + 1);
            incoming[to] -= 1;
            if incoming[to] == 0 {
                ready.push(to);
            }
        }
    }
    rank
}

/// Edge list with one back edge removed per cycle, found by DFS, so ranking
/// always terminates. Back edges are still drawn, just routed against flow.
fn forward_edges(graph: &Graph) -> Vec<(usize, usize)> {
    const UNSEEN: u8 = 0;
    const ACTIVE: u8 = 1;
    const DONE: u8 = 2;

    let count = graph.nodes.len();
    let mut adjacency = vec![Vec::new(); count];
    for (idx, edge) in graph.edges.iter().enumerate() {
        adjacency[edge.from].push((edge.to, idx));
    }

    let mut state = vec![UNSEEN; count];
    let mut back = vec![false; graph.edges.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for root in 0..count {
        if state[root] != UNSEEN {
            continue;
        }
        state[root] = ACTIVE;
        stack.push((root, 0));
        while let Some((node, cursor)) = stack.pop() {
            match adjacency[node].get(cursor) {
                None => state[node] = DONE,
                Some(&(next, idx)) => {
                    stack.push((node, cursor + 1));
                    match state[next] {
                        ACTIVE => back[idx] = true,
                        UNSEEN => {
                            state[next] = ACTIVE;
                            stack.push((next, 0));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    graph
        .edges
        .iter()
        .enumerate()
        .filter(|(idx, _)| !back[*idx])
        .map(|(_, edge)| (edge.from, edge.to))
        .collect()
}

/// Angled shapes need a cell each side for the slant; round ones read better
/// with the same breathing room.
fn shape_padding(shape: Shape) -> usize {
    match shape {
        Shape::Rhombus | Shape::Hexagon | Shape::Circle => LABEL_PAD + 1,
        // The inner bars of a subroutine frame need their own cells.
        Shape::Subroutine => LABEL_PAD + 2,
        _ => LABEL_PAD,
    }
}

/// Maps `along`/`across` onto screen cells for one of the four directions.
struct Mapper {
    direction: Direction,
    along_total: usize,
}

impl Mapper {
    fn point(&self, along: usize, across: usize) -> (usize, usize) {
        match self.direction {
            Direction::Down => (across, along),
            Direction::Up => (across, self.flip(along, 1)),
            Direction::Right => (along, across),
            Direction::Left => (self.flip(along, 1), across),
        }
    }

    fn rect(&self, along: usize, across: usize, along_size: usize) -> (usize, usize) {
        match self.direction {
            Direction::Down => (across, along),
            Direction::Up => (across, self.flip(along, along_size)),
            Direction::Right => (along, across),
            Direction::Left => (self.flip(along, along_size), across),
        }
    }

    fn flip(&self, along: usize, size: usize) -> usize {
        self.along_total.saturating_sub(along + size)
    }

    fn arrow(&self, descending: bool) -> Arrow {
        match (self.direction, descending) {
            (Direction::Down, true) | (Direction::Up, false) => Arrow::Down,
            (Direction::Down, false) | (Direction::Up, true) => Arrow::Up,
            (Direction::Right, true) | (Direction::Left, false) => Arrow::Right,
            (Direction::Right, false) | (Direction::Left, true) => Arrow::Left,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::*;
    use test_case::test_case;

    const CYCLE: &str = "flowchart TD\n A --> B --> C --> A";
    const DIAMOND: &str = "flowchart TD\n A --> B & C\n B & C --> D";
    const CLUSTERED: &str =
        "flowchart TD\n Start --> A\n subgraph Box\n  A --> B\n end\n B --> Done";
    /// Two subgraphs whose rank spans overlap, outsiders between and after
    /// them, four edges converging on one node, and members of unequal width.
    /// Every one of those used to break a different geometric invariant.
    const PIPELINE: &str = "graph LR\n\
        \x20   subgraph corpora[\"datasets/\"]\n\
        \x20       CP[\"caveman_pirate<br/>1,200 authored rows\"]\n\
        \x20       CS[\"lora_sft_120k<br/>manifest only\"]\n\
        \x20   end\n\
        \x20   subgraph pkg[\"src/llmdata/\"]\n\
        \x20       MIX[\"data_mix<br/>weighted mixing, streaming\"]\n\
        \x20       TL[\"train_lora\"]\n\
        \x20       TG[\"train_grpo\"]\n\
        \x20       PROF[\"targets/<br/>site table, rank ceiling\"]\n\
        \x20   end\n\
        \x20   HF[(\"HuggingFace Hub<br/>/ local cache\")]\n\
        \x20   ADP[/\"PEFT adapter directory<br/>+ training_report.json\"/]\n\
        \x20   RT[\"serving runtime<br/>(separate repository)\"]\n\
        \x20   CP --> TL\n\
        \x20   CS --> HF --> TL\n\
        \x20   MIX --> TL\n\
        \x20   MIX --> TG\n\
        \x20   PROF --> TL\n\
        \x20   PROF --> TG\n\
        \x20   TL --> ADP\n\
        \x20   TG --> ADP\n\
        \x20   ADP -.->|convert, then serve| RT\n";

    fn place(source: &str) -> Layout {
        layout(&parse::parse(source).expect("fixture should parse"))
    }

    /// Cells two inclusive intervals have in common.
    fn overlap((a_lo, a_hi): (usize, usize), (b_lo, b_hi): (usize, usize)) -> usize {
        let (lo, hi) = (a_lo.max(b_lo), a_hi.min(b_hi));
        hi.saturating_sub(lo) + usize::from(lo <= hi)
    }

    fn rects_overlap(
        (ax, ay, aw, ah): (usize, usize, usize, usize),
        (bx, by, bw, bh): (usize, usize, usize, usize),
    ) -> bool {
        ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
    }

    fn node_rect(node: &PlacedNode) -> (usize, usize, usize, usize) {
        (node.x, node.y, node.width, node.height)
    }

    /// Whether a cell lies on the shape as drawn, allowing `reach` cells of
    /// clearance beside a side. Upright borders carry a junction glyph so a
    /// line lands on them, while a slant has none and is met head on.
    fn near_outline(shape: Shape, node: &PlacedNode, x: usize, y: usize, reach: isize) -> bool {
        if !(node.y..node.y + node.height).contains(&y) {
            return false;
        }
        let row = y - node.y;
        let (left, right) = shape.sides(node.width, node.height, row);
        let (left, right) = (node.x + left, node.x + right);
        let sideways = [-reach, reach]
            .into_iter()
            .zip([left, right])
            .any(|(off, border)| Some(x) == border.checked_add_signed(off));
        let endwise = (row == 0 || row + 1 == node.height) && (left..=right).contains(&x);
        sideways || endwise
    }

    fn cluster_rect(cluster: &PlacedCluster) -> (usize, usize, usize, usize) {
        (cluster.x, cluster.y, cluster.width, cluster.height)
    }

    #[test]
    fn the_pipeline_parses_with_the_memberships_it_declares() {
        let graph = parse::parse(PIPELINE).expect("fixture should parse");
        let named = |title: &str| {
            let group = graph
                .subgraphs
                .iter()
                .find(|group| group.title == title)
                .unwrap_or_else(|| panic!("no subgraph {title:?}"));
            let mut labels: Vec<String> = group
                .nodes
                .iter()
                .map(|&idx| graph.nodes[idx].label[0].clone())
                .collect();
            labels.sort();
            labels
        };
        assert_eq!(
            named("datasets/"),
            ["caveman_pirate", "lora_sft_120k"]
        );
        assert_eq!(
            named("src/llmdata/"),
            ["data_mix", "targets/", "train_grpo", "train_lora"]
        );
        // The outsiders are only ever named after both groups close, so no
        // subgraph may claim them.
        let claimed: Vec<usize> = graph
            .subgraphs
            .iter()
            .flat_map(|group| group.nodes.iter().copied())
            .collect();
        for label in [
            "HuggingFace Hub",
            "PEFT adapter directory",
            "serving runtime",
        ] {
            let idx = graph
                .nodes
                .iter()
                .position(|node| node.label[0].starts_with(label) || node.label[0].contains(label))
                .unwrap_or_else(|| panic!("no node {label:?}"));
            assert!(!claimed.contains(&idx), "{label:?} must stay outside");
        }
    }

    #[test_case(PIPELINE  ; "pipeline")]
    #[test_case(CLUSTERED ; "clustered")]
    fn subgraph_frames_never_overlap_each_other(source: &str) {
        let placed = place(source);
        for (i, cluster) in placed.clusters.iter().enumerate() {
            for other in &placed.clusters[i + 1..] {
                assert!(
                    !rects_overlap(cluster_rect(cluster), cluster_rect(other)),
                    "{cluster:?} overlaps {other:?}"
                );
            }
        }
    }

    #[test_case(PIPELINE  ; "pipeline")]
    #[test_case(CLUSTERED ; "clustered")]
    fn a_subgraph_frame_holds_its_members_and_no_one_else(source: &str) {
        let graph = parse::parse(source).expect("fixture should parse");
        let placed = place(source);
        for cluster in &placed.clusters {
            let members = &graph.subgraphs[cluster.subgraph].nodes;
            for node in &placed.nodes {
                let rect = node_rect(node);
                let touches = rects_overlap(rect, cluster_rect(cluster));
                let contained = node.x >= cluster.x
                    && node.y >= cluster.y
                    && node.x + node.width <= cluster.x + cluster.width
                    && node.y + node.height <= cluster.y + cluster.height;
                match members.contains(&node.node) {
                    // A member has to be wholly inside, not merely touching.
                    true => assert!(
                        contained,
                        "member {:?} escapes {:?}",
                        graph.nodes[node.node].label, graph.subgraphs[cluster.subgraph].title
                    ),
                    // An outsider must not even clip the frame.
                    false => assert!(
                        !touches,
                        "{:?} intrudes on {:?}",
                        graph.nodes[node.node].label, graph.subgraphs[cluster.subgraph].title
                    ),
                }
            }
        }
    }

    /// Both leaning families, entered and left on more than one row, so a
    /// border that moves per row cannot be mistaken for the bounding box.
    const LEANING: &str = "flowchart LR\n\
        A[Start] --> B[/\"Read rows<br/>and headers\"/]\n\
        C[Also] --> B\n\
        B --> D[\\Mirror\\]\n\
        B --> E[/Widen\\]\n\
        D --> F[End]\n\
        E --> F";

    #[test_case(PIPELINE ; "pipeline")]
    #[test_case(DIAMOND  ; "diamond")]
    #[test_case(CYCLE    ; "cycle")]
    #[test_case(LEANING  ; "leaning")]
    fn every_edge_starts_on_the_border_of_its_own_source(source: &str) {
        let graph = parse::parse(source).expect("fixture should parse");
        let placed = place(source);
        for (edge, wire) in graph.edges.iter().zip(&placed.edges) {
            let node = placed
                .nodes
                .iter()
                .find(|node| node.node == edge.from)
                .expect("source is placed");
            let &(x, y) = wire.points.first().expect("a route has points");
            let shape = graph.nodes[edge.from].shape;
            let reach = isize::from(shape.lean().is_some());
            assert!(
                near_outline(shape, node, x, y, reach),
                "{:?} leaves from ({x},{y}), off the outline of {:?}",
                graph.nodes[edge.from].label,
                node
            );
        }
    }

    #[test_case(PIPELINE ; "pipeline")]
    #[test_case(DIAMOND  ; "diamond")]
    fn converging_edges_keep_one_arrow_each(source: &str) {
        let placed = place(source);
        let heads: Vec<(usize, usize)> = placed
            .edges
            .iter()
            .filter_map(|edge| edge.arrow.map(|(x, y, _)| (x, y)))
            .collect();
        for (i, head) in heads.iter().enumerate() {
            assert!(
                !heads[i + 1..].contains(head),
                "two arrows share {head:?}: {heads:?}"
            );
        }
    }

    #[test_case(PIPELINE ; "pipeline")]
    fn no_route_runs_through_a_node(source: &str) {
        let graph = parse::parse(source).expect("fixture should parse");
        let placed = place(source);
        for (edge, wire) in graph.edges.iter().zip(&placed.edges) {
            for pair in wire.points.windows(2) {
                let (from, to) = (pair[0], pair[1]);
                let xs = from.0.min(to.0)..=from.0.max(to.0);
                let ys = from.1.min(to.1)..=from.1.max(to.1);
                for node in &placed.nodes {
                    if node.node == edge.from || node.node == edge.to {
                        continue;
                    }
                    let inside = xs.clone().any(|x| {
                        ys.clone().any(|y| {
                            x > node.x
                                && x < node.x + node.width - 1
                                && y > node.y
                                && y < node.y + node.height - 1
                        })
                    });
                    assert!(
                        !inside,
                        "a route crosses the inside of {:?}",
                        graph.nodes[node.node].label
                    );
                }
            }
        }
    }

    /// A decision with two labelled branches used to place both labels in the
    /// same gap, so one overwrote the other.
    const BRANCHES: &str = "flowchart LR\n A{Ok?} -->|No| B[Fix]\n A -->|Yes| C[Ship]";
    /// The label sits in a gap that a returning edge also runs through.
    const LOOPING: &str =
        "flowchart LR\n A[Run] --> B{Pass?}\n B -->|No| C[Debug]\n C --> A\n B -->|Yes| D[Ship]";

    fn label_rects(layout: &Layout) -> Vec<(usize, usize, usize, &str)> {
        layout
            .edges
            .iter()
            .filter_map(|edge| edge.label.as_ref())
            .map(|label| (label.x, label.y, label.text.width(), label.text.as_str()))
            .collect()
    }

    #[test_case(BRANCHES ; "two_branches_out_of_one_decision")]
    #[test_case(LOOPING  ; "a_branch_beside_a_returning_edge")]
    #[test_case(DIAMOND  ; "a_diamond")]
    fn edge_labels_never_share_a_cell(source: &str) {
        let layout = place(source);
        let labels = label_rects(&layout);
        for (i, &(ax, ay, aw, at)) in labels.iter().enumerate() {
            for &(bx, by, bw, bt) in &labels[i + 1..] {
                assert!(
                    ay != by || ax + aw <= bx || bx + bw <= ax,
                    "{at:?} and {bt:?} overlap in {source:?}: {labels:?}"
                );
            }
        }
    }

    #[test_case(BRANCHES ; "two_branches_out_of_one_decision")]
    #[test_case(LOOPING  ; "a_branch_beside_a_returning_edge")]
    fn an_edge_label_never_lands_on_a_box(source: &str) {
        let layout = place(source);
        for (x, y, width, text) in label_rects(&layout) {
            for node in &layout.nodes {
                assert!(
                    x + width <= node.x
                        || node.x + node.width <= x
                        || y < node.y
                        || node.y + node.height <= y,
                    "{text:?} lands on a box in {source:?}"
                );
            }
        }
    }

    fn overlaps(a: &PlacedNode, b: &PlacedNode) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test_case(CYCLE     ; "cycle")]
    #[test_case(DIAMOND   ; "diamond")]
    #[test_case(CLUSTERED ; "clustered")]
    fn layout_is_deterministic(source: &str) {
        assert_eq!(place(source), place(source));
    }

    #[test_case("flowchart TD\n A --> B & C & D"                    ; "fan_out")]
    #[test_case(DIAMOND                                             ; "diamond")]
    #[test_case(CYCLE                                               ; "cycle")]
    #[test_case(CLUSTERED                                           ; "clustered")]
    #[test_case("flowchart LR\n A --> B --> C --> D\n A --> D"      ; "long_edge_lr")]
    fn boxes_never_overlap(source: &str) {
        let placed = place(source);
        for (index, node) in placed.nodes.iter().enumerate() {
            for other in &placed.nodes[index + 1..] {
                assert!(!overlaps(node, other), "{node:?} overlaps {other:?}");
            }
        }
    }

    #[test_case("flowchart TD\n A --> B & C & D"                    ; "fan_out")]
    #[test_case(DIAMOND                                             ; "diamond")]
    #[test_case(CLUSTERED                                           ; "clustered")]
    #[test_case("flowchart LR\n A --> B --> C --> D\n A --> D"      ; "long_edge_lr")]
    fn everything_fits_inside_the_reported_extent(source: &str) {
        let placed = place(source);
        for node in &placed.nodes {
            assert!(node.x + node.width <= placed.width, "{node:?}");
            assert!(node.y + node.height <= placed.height, "{node:?}");
        }
        for edge in &placed.edges {
            for &(x, y) in &edge.points {
                assert!(x < placed.width && y < placed.height, "{:?}", edge.points);
            }
        }
    }

    #[test]
    fn cluster_encloses_its_members_and_nothing_else() {
        let graph = parse::parse(CLUSTERED).expect("fixture should parse");
        let placed = layout(&graph);
        let cluster = placed.clusters.first().expect("one subgraph");
        let members = &graph.subgraphs[cluster.subgraph].nodes;
        for node in &placed.nodes {
            let inside = node.x >= cluster.x
                && node.y >= cluster.y
                && node.x + node.width <= cluster.x + cluster.width
                && node.y + node.height <= cluster.y + cluster.height;
            assert_eq!(
                inside,
                members.contains(&node.node),
                "node {} placement disagrees with subgraph membership",
                node.node
            );
        }
    }

    #[test]
    fn ranks_follow_the_arrows() {
        let placed = place("flowchart TD\n A --> B --> C");
        let tops: Vec<usize> = placed.nodes.iter().map(|node| node.y).collect();
        assert!(tops[0] < tops[1] && tops[1] < tops[2], "{tops:?}");
    }

    #[test_case(parse::Direction::Right ; "lr")]
    #[test_case(parse::Direction::Left  ; "rl")]
    fn horizontal_directions_advance_along_x(direction: parse::Direction) {
        let keyword = match direction {
            parse::Direction::Right => "LR",
            _ => "RL",
        };
        let placed = place(&format!("flowchart {keyword}\n A --> B"));
        assert_ne!(placed.nodes[0].x, placed.nodes[1].x);
        assert_eq!(placed.nodes[0].y, placed.nodes[1].y);
    }

    #[test]
    fn reversed_direction_mirrors_the_ranking() {
        let down = place("flowchart TD\n A --> B");
        let up = place("flowchart BT\n A --> B");
        assert!(down.nodes[0].y < down.nodes[1].y);
        assert!(up.nodes[0].y > up.nodes[1].y);
    }

    #[test]
    fn a_cycle_still_ranks_every_node_apart() {
        let placed = place(CYCLE);
        let mut tops: Vec<usize> = placed.nodes.iter().map(|node| node.y).collect();
        tops.sort_unstable();
        tops.dedup();
        assert_eq!(tops.len(), 3, "each node in the cycle needs its own rank");
    }

    #[test]
    fn open_links_carry_no_arrow() {
        let placed = place("flowchart TD\n A --- B");
        assert!(placed.edges.iter().all(|edge| edge.arrow.is_none()));
    }

    #[test]
    fn polyline_segments_are_orthogonal() {
        let placed = place(DIAMOND);
        for edge in &placed.edges {
            for pair in edge.points.windows(2) {
                let (from, to) = (pair[0], pair[1]);
                assert!(
                    from.0 == to.0 || from.1 == to.1,
                    "diagonal segment {from:?} -> {to:?}"
                );
            }
        }
    }
    #[test_case(PIPELINE ; "pipeline")]
    #[test_case(LEANING  ; "leaning")]
    fn every_arrow_points_at_the_outline_it_terminates_on(source: &str) {
        let graph = parse(source).expect("fixture should parse");
        let placed = place(source);
        for (edge, wire) in graph.edges.iter().zip(&placed.edges) {
            let Some((x, y, arrow)) = wire.arrow else {
                continue;
            };
            let node = placed
                .nodes
                .iter()
                .find(|node| node.node == edge.to)
                .expect("target is placed");
            let (dx, dy): (isize, isize) = match arrow {
                Arrow::Right => (1, 0),
                Arrow::Left => (-1, 0),
                Arrow::Down => (0, 1),
                Arrow::Up => (0, -1),
            };
            let pointed = (x.checked_add_signed(dx), y.checked_add_signed(dy));
            let (Some(px), Some(py)) = pointed else {
                panic!("head at ({x},{y}) points off the canvas");
            };
            assert!(
                near_outline(graph.nodes[edge.to].shape, node, px, py, 0),
                "{:?} takes an arrow at ({x},{y}) pointing at ({px},{py}), \
                 which is not on the outline of {node:?}",
                graph.nodes[edge.to].label,
            );
        }
    }

    #[test]
    fn a_slant_widens_a_node_instead_of_crowding_its_text() {
        let upright = place("flowchart LR\n  A[\"Read rows<br/>and headers\"] --> B[End]");
        let leaning = place("flowchart LR\n  A[/\"Read rows<br/>and headers\"/] --> B[End]");
        let (upright, leaning) = (&upright.nodes[0], &leaning.nodes[0]);
        assert_eq!(upright.height, leaning.height, "a slant costs no rows");
        assert_eq!(
            leaning.width - upright.width,
            leaning.height - 1,
            "a parallelogram pays one column per row of shear"
        );
    }

    /// A frame may be crossed, since an edge that leaves a subgraph has to get
    /// out, but an edge that runs along one erases it.
    #[test_case(PIPELINE  ; "pipeline")]
    #[test_case(CLUSTERED ; "clustered")]
    fn no_edge_runs_along_a_subgraph_border(source: &str) {
        const CROSSING: usize = 1;
        let placed = place(source);
        for wire in &placed.edges {
            for pair in wire.points.windows(2) {
                let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
                let (xs, ys) = ((x0.min(x1), x0.max(x1)), (y0.min(y1), y0.max(y1)));
                for cluster in &placed.clusters {
                    let right = cluster.x + cluster.width - 1;
                    let bottom = cluster.y + cluster.height - 1;
                    let shared = match (y0 == y1, x0 == x1) {
                        (true, _) if y0 == cluster.y || y0 == bottom => {
                            overlap(xs, (cluster.x, right))
                        }
                        (_, true) if x0 == cluster.x || x0 == right => {
                            overlap(ys, (cluster.y, bottom))
                        }
                        _ => 0,
                    };
                    assert!(
                        shared <= CROSSING,
                        "an edge shares {shared} cells with the border of {cluster:?} \
                         between ({x0},{y0}) and ({x1},{y1})"
                    );
                }
            }
        }
    }
}
