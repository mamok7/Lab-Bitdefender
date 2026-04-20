use std::io;
use serde::Deserialize;
use std::env;
use std::fs::File;

#[derive(Debug, Deserialize)] 

struct GridCell{
    x: usize, 
    y: usize,
}

#[derive(Debug, Deserialize)]

struct Labyrinth{
    width: usize, 
    height: usize, 
    start: GridCell,
    goal: GridCell,
    grid: Vec<GridCell>,
}

fn main() -> io::Result<()>{
    let args: Vec<String> = env::args().collect();
    println!("Argumente: {:?}", args);
    
    let file = File::open(&args[1])?;
    println!("Am deschis: {:?}", file);

    let labyrinth: Labyrinth = serde_json::from_reader(file)?;
    println!("labirintul este:{:?}", labyrinth);
    
    Ok(())

}
