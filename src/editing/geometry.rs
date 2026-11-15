// Copyright (c) 2026 Replexo
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Straightening and perspective correction
//!
//! The warp is applied to the image before crop, rotate and flip. The image
//! keeps its size and is zoomed in just enough that no empty corners show.
//!
//! The warp works in centred coordinates scaled by half the image diagonal,
//! which stay the same when the image is turned by 90°. In them, a
//! perspective `p` maps `u` to `u / (1 + p·u)` and is followed by the
//! rotation.

use glycin::{Operation, Operations};
use gufo::common::orientation::Rotation;
use image::RgbaImage;

use super::markup::Shape;

/// Largest straightening angle in degrees
pub const MAX_ANGLE: f64 = 45.;
/// Largest perspective factor, which keeps the image clear of its vanishing
/// line
const MAX_PERSPECTIVE: f64 = 0.4;

/// 3×3 matrix acting on column vectors
pub type Matrix = [[f64; 3]; 3];

/// The sliders of the geometry, in the order they are shown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Property {
    Straighten,
    Vertical,
    Horizontal,
}

/// Straightening and perspective, in the orientation of the original image
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Geometry {
    /// Clockwise rotation in degrees
    pub angle: f64,
    /// Perspective along x and y
    pub perspective: (f64, f64),
}

impl Geometry {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// Slider value from -1 to 1
    pub fn get(&self, property: Property) -> f64 {
        match property {
            Property::Straighten => self.angle / MAX_ANGLE,
            Property::Vertical => self.perspective.1 / MAX_PERSPECTIVE,
            Property::Horizontal => self.perspective.0 / MAX_PERSPECTIVE,
        }
    }

    pub fn set(&mut self, property: Property, value: f64) {
        let value = value.clamp(-1., 1.);
        match property {
            Property::Straighten => self.angle = value * MAX_ANGLE,
            Property::Vertical => self.perspective.1 = value * MAX_PERSPECTIVE,
            Property::Horizontal => self.perspective.0 = value * MAX_PERSPECTIVE,
        }
    }

    /// The geometry as seen after the rotations and flips in `operations`
    pub fn to_view(self, operations: Option<&Operations>) -> Self {
        let (turn, det) = orientation(operations);
        Self {
            angle: self.angle * det,
            perspective: apply2(&turn, self.perspective),
        }
    }

    /// The geometry for the original image, from one seen after the
    /// rotations and flips in `operations`
    pub fn from_view(view: Self, operations: Option<&Operations>) -> Self {
        let (turn, det) = orientation(operations);
        // Turns and flips are orthogonal, so the transpose inverts them
        let back = [[turn[0][0], turn[1][0]], [turn[0][1], turn[1][1]]];
        Self {
            angle: view.angle * det,
            perspective: apply2(&back, view.perspective),
        }
    }

    /// Maps pixel coordinates of the image of `size` to pixel coordinates
    /// of the result
    pub fn matrix(&self, (width, height): (f64, f64)) -> Matrix {
        let scale = width.hypot(height) / 2.;
        let (a, b) = self.perspective;
        let perspective = [[1., 0., 0.], [0., 1., 0.], [a, b, 1.]];
        let (sin, cos) = self.angle.to_radians().sin_cos();
        let rotation = [[cos, -sin, 0.], [sin, cos, 0.], [0., 0., 1.]];
        let warp = multiply(&rotation, &perspective);

        // Largest factor the corners of the result can be pulled out by
        // while staying inside the warped image
        let (w, h) = (width / 2. / scale, height / 2. / scale);
        let rect = [(-w, -h), (w, -h), (w, h), (-w, h)];
        let quad = rect.map(|p| transform(&warp, p));
        let mut fit = f64::MAX;
        for i in 0..4 {
            let (p, q) = (quad[i], quad[(i + 1) % 4]);
            // Edge as n·x = d, with the centre inside
            let mut n = (q.1 - p.1, p.0 - q.0);
            let mut d = n.0 * p.0 + n.1 * p.1;
            if d < 0. {
                n = (-n.0, -n.1);
                d = -d;
            }
            for c in rect {
                let reach = n.0 * c.0 + n.1 * c.1;
                if reach > 0. {
                    fit = fit.min(d / reach);
                }
            }
        }
        let zoom = if fit.is_finite() && fit > 0. {
            1. / fit
        } else {
            1.
        };
        let zoom = [[zoom, 0., 0.], [0., zoom, 0.], [0., 0., 1.]];

        let to_centred = [
            [1. / scale, 0., -width / 2. / scale],
            [0., 1. / scale, -height / 2. / scale],
            [0., 0., 1.],
        ];
        let from_centred = [
            [scale, 0., width / 2.],
            [0., scale, height / 2.],
            [0., 0., 1.],
        ];
        multiply(
            &from_centred,
            &multiply(&zoom, &multiply(&warp, &to_centred)),
        )
    }

    /// Warps `image` with bilinear sampling
    pub fn apply(&self, image: &RgbaImage) -> RgbaImage {
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 {
            return image.clone();
        }
        let Some(inverse) = invert(&self.matrix((width as f64, height as f64))) else {
            return image.clone();
        };

        // Rows are split between threads
        let mut data = vec![0u8; width as usize * height as usize * 4];
        let row_len = width as usize * 4;
        let threads = std::thread::available_parallelism().map_or(1, |x| x.get());
        let rows_per_thread = (height as usize).div_ceil(threads).max(1);
        std::thread::scope(|scope| {
            for (chunk_index, chunk) in data.chunks_mut(rows_per_thread * row_len).enumerate() {
                let inverse = &inverse;
                scope.spawn(move || {
                    for (row_index, row) in chunk.chunks_mut(row_len).enumerate() {
                        let y = (chunk_index * rows_per_thread + row_index) as f64 + 0.5;
                        for (x, pixel) in row.chunks_mut(4).enumerate() {
                            let (sx, sy) = transform(inverse, (x as f64 + 0.5, y));
                            pixel.copy_from_slice(&sample(image, sx - 0.5, sy - 0.5));
                        }
                    }
                });
            }
        });
        RgbaImage::from_raw(width, height, data).unwrap_or_else(|| image.clone())
    }
}

/// Moves `shapes` drawn on the image warped by `from` so they sit on the
/// same spot of the image warped by `to`
///
/// Straightening moves shapes exactly. A perspective change is followed
/// exactly at each shape's centre and closely around it, since shapes only
/// take affine transformations.
pub fn follow(shapes: &[Shape], from: Geometry, to: Geometry, size: (f64, f64)) -> Vec<Shape> {
    let Some(back) = invert(&from.matrix(size)) else {
        return shapes.to_vec();
    };
    let step = multiply(&to.matrix(size), &back);

    shapes
        .iter()
        .map(|shape| {
            let (x0, y0, x1, y1) = shape.bounds();
            let p = shape
                .matrix
                .transform_point((x0 + x1) / 2., (y0 + y1) / 2.);
            let q = transform(&step, p);
            // Slope of the warp at the centre
            let e = 1.;
            let (right, left) = (
                transform(&step, (p.0 + e, p.1)),
                transform(&step, (p.0 - e, p.1)),
            );
            let (down, up) = (
                transform(&step, (p.0, p.1 + e)),
                transform(&step, (p.0, p.1 - e)),
            );
            let (xx, yx) = ((right.0 - left.0) / (2. * e), (right.1 - left.1) / (2. * e));
            let (xy, yy) = ((down.0 - up.0) / (2. * e), (down.1 - up.1) / (2. * e));
            let local = cairo::Matrix::new(
                xx,
                yx,
                xy,
                yy,
                q.0 - xx * p.0 - xy * p.1,
                q.1 - yx * p.0 - yy * p.1,
            );

            let mut shape = shape.clone();
            shape.matrix = cairo::Matrix::multiply(&shape.matrix, &local);
            shape
        })
        .collect()
}

/// Bilinear sample on premultiplied colours, transparent outside the image
fn sample(image: &RgbaImage, x: f64, y: f64) -> [u8; 4] {
    let (width, height) = (image.width() as i64, image.height() as i64);
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);

    let mut sum = [0f64; 4];
    for (dx, dy, weight) in [
        (0, 0, (1. - fx) * (1. - fy)),
        (1, 0, fx * (1. - fy)),
        (0, 1, (1. - fx) * fy),
        (1, 1, fx * fy),
    ] {
        // Edge pixels are repeated, so the border doesn't fade out
        let px = (x0 + dx).clamp(0, width - 1);
        let py = (y0 + dy).clamp(0, height - 1);
        if x0 + dx < -1 || y0 + dy < -1 || x0 + dx > width || y0 + dy > height {
            continue;
        }
        let [r, g, b, a] = image.get_pixel(px as u32, py as u32).0.map(|c| c as f64);
        let alpha = a / 255.;
        sum[0] += r * alpha * weight;
        sum[1] += g * alpha * weight;
        sum[2] += b * alpha * weight;
        sum[3] += a * weight;
    }

    let alpha = sum[3] / 255.;
    let straight = |c: f64| {
        if alpha > 0. {
            (c / alpha).round().clamp(0., 255.) as u8
        } else {
            0
        }
    };
    [
        straight(sum[0]),
        straight(sum[1]),
        straight(sum[2]),
        sum[3].round().clamp(0., 255.) as u8,
    ]
}

/// The turns and flips in `operations` as a 2×2 matrix on centred
/// coordinates, and its determinant
fn orientation(operations: Option<&Operations>) -> ([[f64; 2]; 2], f64) {
    let mut turn = [[1., 0.], [0., 1.]];
    for operation in operations.map(|x| x.operations()).unwrap_or_default() {
        // Screen coordinates, with y pointing down
        let step = match operation {
            Operation::MirrorHorizontally => [[-1., 0.], [0., 1.]],
            Operation::MirrorVertically => [[1., 0.], [0., -1.]],
            Operation::Rotate(Rotation::_90) => [[0., 1.], [-1., 0.]],
            Operation::Rotate(Rotation::_180) => [[-1., 0.], [0., -1.]],
            Operation::Rotate(Rotation::_270) => [[0., -1.], [1., 0.]],
            _ => continue,
        };
        turn = [
            [
                step[0][0] * turn[0][0] + step[0][1] * turn[1][0],
                step[0][0] * turn[0][1] + step[0][1] * turn[1][1],
            ],
            [
                step[1][0] * turn[0][0] + step[1][1] * turn[1][0],
                step[1][0] * turn[0][1] + step[1][1] * turn[1][1],
            ],
        ];
    }
    let det = turn[0][0] * turn[1][1] - turn[0][1] * turn[1][0];
    (turn, det)
}

fn apply2(m: &[[f64; 2]; 2], (x, y): (f64, f64)) -> (f64, f64) {
    // Avoids -0 from flips, so an untouched geometry stays the identity
    let clean = |v: f64| if v == 0. { 0. } else { v };
    (
        clean(m[0][0] * x + m[0][1] * y),
        clean(m[1][0] * x + m[1][1] * y),
    )
}

pub fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [[0.; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, value) in row.iter_mut().enumerate() {
            *value = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

pub fn transform(m: &Matrix, (x, y): (f64, f64)) -> (f64, f64) {
    let w = m[2][0] * x + m[2][1] * y + m[2][2];
    (
        (m[0][0] * x + m[0][1] * y + m[0][2]) / w,
        (m[1][0] * x + m[1][1] * y + m[1][2]) / w,
    )
}

pub fn invert(m: &Matrix) -> Option<Matrix> {
    let cofactor =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let det = m[0][0] * cofactor(1, 2, 1, 2) - m[0][1] * cofactor(1, 2, 0, 2)
        + m[0][2] * cofactor(1, 2, 0, 1);
    if det.abs() < 1e-12 {
        return None;
    }
    Some([
        [
            cofactor(1, 2, 1, 2) / det,
            -cofactor(0, 2, 1, 2) / det,
            cofactor(0, 1, 1, 2) / det,
        ],
        [
            -cofactor(1, 2, 0, 2) / det,
            cofactor(0, 2, 0, 2) / det,
            -cofactor(0, 1, 0, 2) / det,
        ],
        [
            cofactor(1, 2, 0, 1) / det,
            -cofactor(0, 2, 0, 1) / det,
            cofactor(0, 1, 0, 1) / det,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn identity_keeps_pixels() {
        let m = Geometry::default().matrix((40., 30.));
        assert!(close(transform(&m, (3., 7.)), (3., 7.)));
    }

    #[test]
    fn corners_stay_inside_the_warped_image() {
        let size = (400., 300.);
        for geometry in [
            Geometry {
                angle: 10.,
                perspective: (0., 0.),
            },
            Geometry {
                angle: -30.,
                perspective: (0.2, -0.3),
            },
            Geometry {
                angle: 0.,
                perspective: (0., 0.4),
            },
        ] {
            let inverse = invert(&geometry.matrix(size)).unwrap();
            for corner in [(0., 0.), (400., 0.), (400., 300.), (0., 300.)] {
                let (x, y) = transform(&inverse, corner);
                assert!(
                    (-1e-6..=400. + 1e-6).contains(&x) && (-1e-6..=300. + 1e-6).contains(&y),
                    "{geometry:?} {corner:?} -> {x} {y}"
                );
            }
        }
    }

    #[test]
    fn rotation_keeps_the_centre() {
        let geometry = Geometry {
            angle: 12.,
            perspective: (0., 0.),
        };
        let m = geometry.matrix((40., 30.));
        assert!(close(transform(&m, (20., 15.)), (20., 15.)));
    }

    #[test]
    fn view_roundtrip() {
        let operations = Operations::new(vec![
            Operation::Rotate(Rotation::_90),
            Operation::MirrorHorizontally,
        ]);
        let geometry = Geometry {
            angle: 5.,
            perspective: (0.1, -0.2),
        };
        let view = geometry.to_view(Some(&operations));
        assert_eq!(view.angle, -5.);
        let back = Geometry::from_view(view, Some(&operations));
        assert!((back.angle - 5.).abs() < 1e-9);
        assert!(close(back.perspective, (0.1, -0.2)));
    }

    #[test]
    fn turning_swaps_vertical_and_horizontal() {
        let operations = Operations::new(vec![Operation::Rotate(Rotation::_90)]);
        let geometry = Geometry {
            angle: 0.,
            perspective: (0., 0.3),
        };
        let view = geometry.to_view(Some(&operations));
        assert!(close(view.perspective, (0.3, 0.)));
    }

    #[test]
    fn shapes_follow_straightening() {
        use crate::editing::markup::{Color, Kind};

        let size = (400., 300.);
        let shape = Shape::new(
            Kind::Line {
                from: (100., 100.),
                to: (140., 100.),
            },
            Color::default(),
            4.,
        );
        let to = Geometry {
            angle: 10.,
            perspective: (0., 0.),
        };
        let moved = follow(&[shape], Geometry::default(), to, size);
        let m = to.matrix(size);
        for point in [(100., 100.), (140., 100.)] {
            let expected = transform(&m, point);
            let got = moved[0].matrix.transform_point(point.0, point.1);
            assert!(close(got, expected), "{got:?} {expected:?}");
        }

        // And back again
        let back = follow(&moved, to, Geometry::default(), size);
        let got = back[0].matrix.transform_point(100., 100.);
        assert!(close(got, (100., 100.)));
    }

    #[test]
    fn warp_keeps_size_and_fills() {
        let image = RgbaImage::from_pixel(30, 20, image::Rgba([10, 200, 30, 255]));
        let geometry = Geometry {
            angle: 20.,
            perspective: (0.1, 0.2),
        };
        let warped = geometry.apply(&image);
        assert_eq!(warped.dimensions(), (30, 20));
        assert!(warped.pixels().all(|p| p.0 == [10, 200, 30, 255]));
    }
}
