//! Форма куска окна после косого разреза: выпуклый многоугольник в
//! координатах окна (y вниз). Разрез выпуклого многоугольника прямой даёт
//! два выпуклых, так что все функции рассчитаны на выпуклые.

use smithay::utils::{Logical, Rectangle};

/// Точка в координатах окна.
pub type Pt = [f64; 2];

/// Почти горизонтальный или вертикальный штрих (в пределах этого угла, рад)
/// режет ровно по оси окна: края кусков остаются пиксельно ровными.
pub const AXIS_SNAP: f64 = 2.0 * std::f64::consts::PI / 180.0;

/// Меньше этой площади (px²) кусок вырожден.
const MIN_AREA: f64 = 1.0;

fn cross(o: Pt, a: Pt, b: Pt) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Углы прямоугольника по кругу.
pub fn rect_outline(r: Rectangle<f64, Logical>) -> Vec<Pt> {
    let (x0, y0) = (r.loc.x, r.loc.y);
    let (x1, y1) = (x0 + r.size.w, y0 + r.size.h);
    vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
}

/// Ориентированная площадь (знак — направление обхода).
fn signed_area(poly: &[Pt]) -> f64 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum::<f64>()
        * 0.5
}

pub fn area(poly: &[Pt]) -> f64 {
    signed_area(poly).abs()
}

/// Охватывающий прямоугольник.
pub fn bounds(poly: &[Pt]) -> Rectangle<f64, Logical> {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in poly {
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    Rectangle::new(
        (x0, y0).into(),
        ((x1 - x0).max(0.0), (y1 - y0).max(0.0)).into(),
    )
}

/// Многоугольник — прямоугольник по осям окна (тогда кусок хранится одним
/// `rect`, без формы).
pub fn is_axis_rect(poly: &[Pt]) -> bool {
    let b = bounds(poly);
    (area(poly) - b.size.w * b.size.h).abs() <= 1e-6 * (b.size.w * b.size.h).max(1.0)
}

/// Точка внутри выпуклого многоугольника (граница — внутри).
pub fn contains(poly: &[Pt], p: Pt) -> bool {
    let sign = signed_area(poly).signum();
    let n = poly.len();
    n >= 3 && (0..n).all(|i| cross(poly[i], poly[(i + 1) % n], p) * sign >= -1e-9)
}

/// Квадрат расстояния от точки до многоугольника (0 — внутри).
pub fn distance_sq(poly: &[Pt], p: Pt) -> f64 {
    if contains(poly, p) {
        return 0.0;
    }
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            let ab = [b[0] - a[0], b[1] - a[1]];
            let len2 = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if len2 > 0.0 {
                (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let q = lerp(a, b, t);
            (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)
        })
        .fold(f64::MAX, f64::min)
}

/// Часть отрезка `a→b` внутри выпуклого многоугольника (Cyrus–Beck):
/// параметры входа и выхода `0 ≤ t0 ≤ t1 ≤ 1`.
pub fn clip_segment(poly: &[Pt], a: Pt, b: Pt) -> Option<(f64, f64)> {
    let sign = signed_area(poly).signum();
    let n = poly.len();
    if n < 3 || sign == 0.0 {
        return None;
    }
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for i in 0..n {
        let (e0, e1) = (poly[i], poly[(i + 1) % n]);
        // Значение «внутри» для точки a и его изменение вдоль отрезка.
        let fa = cross(e0, e1, a) * sign;
        let fb = cross(e0, e1, b) * sign;
        let df = fb - fa;
        if df.abs() < 1e-12 {
            if fa < 0.0 {
                return None;
            }
            continue;
        }
        let t = -fa / df;
        if df > 0.0 {
            t0 = t0.max(t);
        } else {
            t1 = t1.min(t);
        }
        if t0 > t1 {
            return None;
        }
    }
    Some((t0, t1))
}

/// Разрез прямой через `a` и `b`: части по обе стороны, обе — больше
/// вырожденной. `None` — прямая не делит многоугольник.
pub fn split(poly: &[Pt], a: Pt, b: Pt) -> Option<(Vec<Pt>, Vec<Pt>)> {
    let n = poly.len();
    if n < 3 || (a[0] - b[0]).hypot(a[1] - b[1]) < 1e-9 {
        return None;
    }
    let side: Vec<f64> = poly.iter().map(|p| cross(a, b, *p)).collect();
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for i in 0..n {
        let j = (i + 1) % n;
        let (p, q) = (poly[i], poly[j]);
        let (sp, sq) = (side[i], side[j]);
        if sp >= 0.0 {
            left.push(p);
        }
        if sp <= 0.0 {
            right.push(p);
        }
        if (sp > 0.0 && sq < 0.0) || (sp < 0.0 && sq > 0.0) {
            let x = lerp(p, q, sp / (sp - sq));
            left.push(x);
            right.push(x);
        }
    }
    (area(&left) >= MIN_AREA && area(&right) >= MIN_AREA).then_some((left, right))
}

/// Толщина многоугольника от прямой через `a` и `b`: расстояние до самой
/// дальней вершины.
pub fn thickness(poly: &[Pt], a: Pt, b: Pt) -> f64 {
    let len = (b[0] - a[0]).hypot(b[1] - a[1]).max(1e-12);
    poly.iter()
        .map(|p| (cross(a, b, *p) / len).abs())
        .fold(0.0, f64::max)
}

/// Выпуклый многоугольник, раздвинутый наружу на `d` (рамка снаружи края).
/// Острые углы срезаются, чтобы рамка не уходила иглой.
pub fn offset(poly: &[Pt], d: f64) -> Vec<Pt> {
    let n = poly.len();
    let sign = signed_area(poly).signum();
    if n < 3 || sign == 0.0 || d == 0.0 {
        return poly.to_vec();
    }
    // Внешняя нормаль ребра i → i+1.
    let normal = |i: usize| -> Pt {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = dx.hypot(dy).max(1e-12);
        [dy / len * sign, -dx / len * sign]
    };
    let mut out = Vec::with_capacity(n * 2);
    for (i, &p) in poly.iter().enumerate() {
        let (n0, n1) = (normal((i + n - 1) % n), normal(i));
        let cos = (n0[0] * n1[0] + n0[1] * n1[1]).clamp(-1.0, 1.0);
        if cos > -0.5 {
            // Угол между нормалями не больше 120°: miter — точка на
            // расстоянии `d` от обоих рёбер.
            let k = d / (1.0 + cos);
            out.push([p[0] + (n0[0] + n1[0]) * k, p[1] + (n0[1] + n1[1]) * k]);
        } else {
            out.push([p[0] + n0[0] * d, p[1] + n0[1] * d]);
            out.push([p[0] + n1[0] * d, p[1] + n1[1] * d]);
        }
    }
    out
}

/// Треугольники веером из вершины 0 (для выпуклого многоугольника).
pub fn fan_indices(n: usize) -> Vec<u16> {
    (1..n.saturating_sub(1))
        .flat_map(|i| [0, i as u16, i as u16 + 1])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Vec<Pt> {
        rect_outline(Rectangle::new((x, y).into(), (w, h).into()))
    }

    #[test]
    fn diagonal_split_halves_a_square() {
        let sq = rect(0.0, 0.0, 100.0, 100.0);
        let (a, b) = split(&sq, [0.0, 0.0], [100.0, 100.0]).unwrap();
        assert!((area(&a) - 5000.0).abs() < 1e-6);
        assert!((area(&b) - 5000.0).abs() < 1e-6);
        assert!(!is_axis_rect(&a) && !is_axis_rect(&b));
        // Каждая часть — ровно треугольник.
        assert_eq!(a.len(), 3);
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn axis_split_keeps_rectangles() {
        let r = rect(10.0, 20.0, 200.0, 100.0);
        let (a, b) = split(&r, [0.0, 50.0], [300.0, 50.0]).unwrap();
        assert!(is_axis_rect(&a) && is_axis_rect(&b));
        let (ba, bb) = (bounds(&a), bounds(&b));
        assert_eq!(ba.size.h + bb.size.h, 100.0);
        assert_eq!(ba.size.w, 200.0);
    }

    #[test]
    fn tiny_slivers_are_allowed_degenerate_are_not() {
        let r = rect(0.0, 0.0, 100.0, 100.0);
        // Отрезать полоску в 2 px можно.
        let (a, b) = split(&r, [0.0, 2.0], [100.0, 2.0]).unwrap();
        assert!((area(&a).min(area(&b)) - 200.0).abs() < 1e-6);
        // Прямая мимо или по краю — не разрез.
        assert!(split(&r, [0.0, 150.0], [100.0, 150.0]).is_none());
        assert!(split(&r, [0.0, 0.0], [100.0, 0.0]).is_none());
    }

    #[test]
    fn clip_and_contains() {
        let tri = vec![[0.0, 0.0], [100.0, 0.0], [0.0, 100.0]];
        assert!(contains(&tri, [10.0, 10.0]));
        assert!(!contains(&tri, [80.0, 80.0]));
        assert_eq!(distance_sq(&tri, [10.0, 10.0]), 0.0);
        assert!((distance_sq(&tri, [-3.0, 10.0]) - 9.0).abs() < 1e-9);
        let (t0, t1) = clip_segment(&tri, [-50.0, 10.0], [150.0, 10.0]).unwrap();
        assert!((t0 - 0.25).abs() < 1e-9 && (t1 - 0.70).abs() < 1e-9);
        assert!(clip_segment(&tri, [60.0, 60.0], [100.0, 100.0]).is_none());
        // Обход в другую сторону — то же самое.
        let rev: Vec<Pt> = tri.iter().rev().copied().collect();
        assert!(contains(&rev, [10.0, 10.0]));
        assert!(clip_segment(&rev, [-50.0, 10.0], [150.0, 10.0]).is_some());
    }

    #[test]
    fn thickness_and_offset() {
        let r = rect(0.0, 0.0, 100.0, 40.0);
        assert!((thickness(&r, [0.0, 0.0], [100.0, 0.0]) - 40.0).abs() < 1e-9);
        let grown = offset(&r, 2.0);
        let b = bounds(&grown);
        assert!((b.loc.x + 2.0).abs() < 1e-9 && (b.size.w - 104.0).abs() < 1e-9);
        assert!((b.size.h - 44.0).abs() < 1e-9);
        assert_eq!(fan_indices(5), vec![0, 1, 2, 0, 2, 3, 0, 3, 4]);
    }
}
