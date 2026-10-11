pub mod shapes;

use std::fmt::Display;
use std::collections::HashMap;

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn double(x: i32) -> i32 {
    add(x, x)
}

pub fn compute(values: &[i32]) -> i32 {
    let total: i32 = values.iter().sum();
    let scaled = total * 2 + 1;
    scaled - 3
}

pub fn triple(x: i32) -> i32 {
    return x * 3;
}

pub fn describe<T: Display>(value: T) -> String {
    format!("value: {value}")
}

pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> String;
}

pub struct Circle {
    pub radius: f64,
}
