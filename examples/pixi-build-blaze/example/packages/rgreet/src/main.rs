fn greet(name: &str) -> String {
    format!("Hello, {name}! (from Rust)")
}

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "world".into());
    println!("{}", greet(&name));
}

#[cfg(test)]
mod tests {
    #[test]
    fn greets() {
        assert_eq!(super::greet("x"), "Hello, x! (from Rust)");
    }
}
