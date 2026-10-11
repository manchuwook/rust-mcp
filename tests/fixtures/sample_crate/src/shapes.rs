use crate::Shape;

/// A square.
pub struct Square {
    pub side: f64,
}

impl Shape for Square {
    fn area(&self) -> f64 {
        self.side * self.side
    }

    fn name(&self) -> String {
        "square".to_string()
    }
}

/// The distance around a square.
pub fn perimeter(square: &Square) -> f64 {
    square.side * 4.0
}

pub fn diagonal(square: &Square) -> f64 {
    square.side * 2.0_f64.sqrt()
}
