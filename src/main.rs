use std::env;

struct Command {
    name: String,
    args: Vec<String>,
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let command = Command {
        name: args[1].clone(),
        args: args[2..].to_vec(),
    };

    println!("command: {}", command.name);
    println!("args: {:?}", command.args);

    // Check client computer

    // User Prompt

    // Streaming. Or shall I say, beaming.
    // With BLAKE3 packet checks

    // Final BLAKE3 check.

    // Run
}
