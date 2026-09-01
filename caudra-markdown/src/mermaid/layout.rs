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

#[derive(Clone, Copy, Debug)]
struct Band {
    start: usize,
    thickness: usize,
}

impl Band {
    fn gap_start(&self) -> usize {
        self.start + self.thickness
    }

    fn last(&self) -> usize {
        self.start + self.thickness - 1
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
                let width = text + 2 * shape_padding(node.shape) + BORDER;
                let height = node.label.len() + BORDER;
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
        let closing = spans.iter().any(|&(_, last)| last == rank);
        let opening = spans.iter().any(|&(first, _)| first == rank + 1);
        (usize::from(closing) + usize::from(opening)) * CLUSTER_PAD
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
    fn assign_ports(&self) -> Vec<(usize, usize)> {
        const NEAR: usize = 0;
        const FAR: usize = 1;

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
            let span = self.vertices[vertex].across_size;
            let base = self.vertices[vertex].across;
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

    fn route(&self, ports: &[(usize, usize)], bands: &[Band], gaps: &[Gap]) -> Vec<Route> {
        let mut routes = Vec::with_capacity(self.hops.len());
        for (idx, hop) in self.hops.iter().enumerate() {
            let (source, target) = (&self.vertices[hop.from], &self.vertices[hop.to]);
            let descending = target.rank > source.rank;
            let low = source.rank.min(target.rank);
            let (near, far) = (&bands[low], &bands[low + 1]);
            let channel = gaps[low]
                .channel
                .iter()
                .find(|(candidate, _)| *candidate == idx)
                .map_or(0, |&(_, row)| row);

            let source_real = matches!(source.kind, Kind::Real(_));
            let target_real = matches!(target.kind, Kind::Real(_));
            let last_hop = self.hops[idx].edge;
            let arrowed = self.graph.edges[last_hop].arrow && target_real;

            // An arrowhead is the terminus: the polyline stops one cell short
            // of the target so the head sits against an unbroken border.
            let (start_along, border_along, arrow_along) = match descending {
                true => (
                    if source_real { near.last() } else { near.start },
                    if target_real { far.start } else { far.last() },
                    arrowed.then(|| far.start - 1),
                ),
                false => (
                    if source_real { far.start } else { far.last() },
                    if target_real { near.last() } else { near.start },
                    arrowed.then(|| near.last() + 1),
                ),
            };
            let end_along = arrow_along.unwrap_or(border_along);

            routes.push(Route {
                hop: idx,
                from_across: ports[idx].0,
                to_across: ports[idx].1,
                start_along,
                channel_along: near.gap_start() + channel,
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

    fn place(source: &str) -> Layout {
        layout(&parse::parse(source).expect("fixture should parse"))
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
}
