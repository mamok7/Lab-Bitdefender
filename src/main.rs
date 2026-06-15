use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use std::collections::HashMap;
use std::collections::VecDeque;

pub const PROTOCOL_VERSION: i32 = 1;

// ─────────────────────────────────────────────────────────────────────────────
// STRUCTURI DE DATE
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerHeroSpawn {
    pub id: i32,
    pub x: i32,
    pub y: i32,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub id: i32,
    pub name: String,
    pub heroes: Vec<PlayerHeroSpawn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeroTypeConfig {
    pub shoot_cooldown: i32,
    pub projectile_ttl: i32,
    pub projectile_speed: i32,
    pub max_hp: i32,
    pub projectile_damage: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameConfig {
    pub width: i32,
    pub height: i32,
    pub turns: i32,
    pub vision_range: i32,
    pub seed: u32,
    pub players: Vec<Player>,
    pub hero_types: HashMap<String, HeroTypeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hero {
    pub id: i32,
    pub owner_id: i32,
    #[serde(rename = "type")]
    pub type_: String,
    pub x: i32,
    pub y: i32,
    pub hp: i32,
    pub cooldown: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wall {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub heroes: Vec<Hero>,
    pub walls: Vec<Wall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartMatchArgs {
    pub match_id: String,
    pub your_player_id: i32,
    pub config: GameConfig,
    pub state: GameState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartTurnArgs {
    pub turn: i32,
    pub state: GameState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndMatchArgs {
    pub reason: String,
    #[serde(default)]
    pub winner: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WsMsg {
    pub command: String,
    pub args: serde_json::Value,
}

// ─────────────────────────────────────────────────────────────────────────────
// FAZE DE JOC
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Rally,
    Attack,
    Hunt,
    /// Tura >= ENDGAME_TURN: toți eroii merg la centrul hărții pentru vision maxim.
    /// Dacă jocul se termină la timp (nu prin kill), câștigă cine vede mai mult.
    Endgame,
}

/// De la această tură încolo botul intră în modul Endgame.
/// Pornim cu 10 ture înainte de limită (config.turns = 200 de obicei → 190).
const ENDGAME_TURN: i32 = 190;

// ─────────────────────────────────────────────────────────────────────────────
// DECIZIA TACTICA PER EROU
// ─────────────────────────────────────────────────────────────────────────────

/// Distanța preferată față de inamic (în unități de joc).
/// Eroii încearcă să rămână la această distanță pentru a putea trage fără a se
/// expune inutil. 15 = ~5 pași de grilă, suficient pentru LoS dar departe de
/// corpo-à-corpo.
const PREFERRED_KITE_DIST: i32 = 15;

/// Dacă distanța față de inamic scade sub această valoare, eroul se retrage
/// prioritar față de orice altceva (chiar și față de tragere).
const TOO_CLOSE_DIST: i32 = 9;

#[derive(Debug, Clone)]
pub enum TacticalAction {
    Shoot { target_x: i32, target_y: i32, comment: &'static str },
    Move  { dest_x: i32, dest_y: i32, comment: &'static str },
}

// ─────────────────────────────────────────────────────────────────────────────
// TRIMITERE MESAJE
// ─────────────────────────────────────────────────────────────────────────────

async fn send_msg<S>(write: &mut S, command: &str, args: serde_json::Value) -> anyhow::Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let json = serde_json::json!({ "command": command, "args": args });
    let text = serde_json::to_string(&json).context("eroare serializare JSON")?;
    println!("  [TRIMIS] {}", text);
    write.send(Message::Text(text.into())).await.context("eroare trimitere mesaj")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// HELPERS
// ─────────────────────────────────────────────────────────────────────────────

fn in_bounds(x: i32, y: i32, map_w: i32, map_h: i32) -> bool {
    x >= 1 && y >= 1 && x < map_w - 1 && y < map_h - 1
}

fn overlaps_wall(cx: i32, cy: i32, walls: &[Wall]) -> bool {
    walls.iter().any(|w| (cx - w.x).abs() < 3 && (cy - w.y).abs() < 3)
}

fn snap_to_grid(x: i32, y: i32) -> (i32, i32) {
    let snap = |v: i32| -> i32 {
        let r = v % 3;
        if r == 1 { v }
        else if r == 0 { v + 1 }
        else { v - 1 }
    };
    (snap(x), snap(y))
}

fn bfs_next_step(
    start_x: i32, start_y: i32,
    target_x: i32, target_y: i32,
    walls: &[Wall],
    map_w: i32, map_h: i32,
) -> (i32, i32) {
    let (target_x, target_y) = snap_to_grid(target_x, target_y);

    if start_x == target_x && start_y == target_y {
        return (start_x, start_y);
    }

    let mut came_from: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    let mut queue: VecDeque<(i32, i32)> = VecDeque::new();

    came_from.insert((start_x, start_y), (start_x, start_y));
    queue.push_back((start_x, start_y));

    let directions: [(i32, i32); 8] = [
        ( 0,  3), ( 0, -3), ( 3,  0), (-3,  0),
        ( 3,  3), ( 3, -3), (-3,  3), (-3, -3),
    ];

    while let Some((cx, cy)) = queue.pop_front() {
        if cx == target_x && cy == target_y {
            let mut current = (cx, cy);
            loop {
                let parent = came_from[&current];
                if parent == (start_x, start_y) {
                    return current;
                }
                current = parent;
            }
        }

        for (dx, dy) in directions {
            let nx = cx + dx;
            let ny = cy + dy;
            let valid = in_bounds(nx, ny, map_w, map_h)
                && !overlaps_wall(nx, ny, walls)
                && !came_from.contains_key(&(nx, ny));
            if valid {
                came_from.insert((nx, ny), (cx, cy));
                queue.push_back((nx, ny));
            }
        }
    }

    (start_x, start_y)
}

// ─────────────────────────────────────────────────────────────────────────────
// BRESENHAM + LINE OF SIGHT
// ─────────────────────────────────────────────────────────────────────────────

fn bresenham_line(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut points = Vec::new();
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        points.push((x, y));
        if x == x1 && y == y1 { break; }
        let e2 = 2 * err;
        if e2 >= dy { err += dy; x += sx; }
        if e2 <= dx { err += dx; y += sy; }
    }
    points
}

fn has_line_of_sight(x0: i32, y0: i32, x1: i32, y1: i32, walls: &[Wall]) -> bool {
    let line = bresenham_line(x0, y0, x1, y1);
    for (px, py) in line {
        for w in walls {
            if (px - w.x).abs() <= 1 && (py - w.y).abs() <= 1 {
                return false;
            }
        }
    }
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// LOOKAHEAD TACTIC — shoot-first cu kiting la distanță
// ─────────────────────────────────────────────────────────────────────────────

/// Verifică dacă inamicul poate trage în eroul nostru de la poziția sa curentă.
fn enemy_can_shoot_hero(enemy: &Hero, hero: &Hero, walls: &[Wall]) -> bool {
    enemy.cooldown == 0 && has_line_of_sight(enemy.x, enemy.y, hero.x, hero.y, walls)
}

/// Distanța Chebyshev (maximul pe x/y) — mai relevantă pe o grilă cu diagonale.
fn dist(ax: i32, ay: i32, bx: i32, by: i32) -> i32 {
    (ax - bx).abs().max((ay - by).abs())
}

/// Calculează pozițiile de kiting: perpendiculare pe linia de foc, preferând
/// poziții care mențin LoS spre inamic și nu intră în ziduri.
fn kite_positions(
    hero: &Hero,
    enemy: &Hero,
    walls: &[Wall],
    map_w: i32,
    map_h: i32,
    keep_los: bool,
) -> Vec<(i32, i32)> {
    let dx = hero.x - enemy.x;
    let dy = hero.y - enemy.y;

    let candidates: [(i32, i32); 6] = [
        (-dy.signum() * 3,  dx.signum() * 3),
        ( dy.signum() * 3, -dx.signum() * 3),
        ( dx.signum() * 3,  dy.signum() * 3),
        (-dy.signum() * 3 + dx.signum() * 3,  dx.signum() * 3 + dy.signum() * 3),
        ( dy.signum() * 3 + dx.signum() * 3, -dx.signum() * 3 + dy.signum() * 3),
        (-dy.signum() * 6,  dx.signum() * 6),
    ];

    let mut positions = Vec::new();
    for (pdx, pdy) in candidates {
        let (sx, sy) = snap_to_grid(hero.x + pdx, hero.y + pdy);
        if !in_bounds(sx, sy, map_w, map_h) { continue; }
        if overlaps_wall(sx, sy, walls) { continue; }
        if keep_los && !has_line_of_sight(sx, sy, enemy.x, enemy.y, walls) { continue; }
        positions.push((sx, sy));
    }
    positions
}

/// Alege acțiunea tactică pentru un singur erou față de cel mai apropiat inamic.
///
/// Prioritatea de decizie:
///   1. Prea aproape (< TOO_CLOSE_DIST) → retrage-te imediat
///   2. cooldown == 0 și am LoS → TRAG întotdeauna, fără excepție
///   3. Pe cooldown și inamicul amenință → kiting lateral (strafing cu LoS)
///   4. Pe cooldown, inamic safe → menții distanța preferată sau strafe
///   5. Fără LoS → BFS prudent spre inamic
fn choose_tactical_action(
    hero: &Hero,
    nearest_enemy: &Hero,
    walls: &[Wall],
    map_w: i32,
    map_h: i32,
    _fallback_dest: (i32, i32),
) -> TacticalAction {
    let d = dist(hero.x, hero.y, nearest_enemy.x, nearest_enemy.y);
    let i_have_los = has_line_of_sight(hero.x, hero.y, nearest_enemy.x, nearest_enemy.y, walls);
    let enemy_threatens = enemy_can_shoot_hero(nearest_enemy, hero, walls);

    // 1. Prea aproape → retragere prioritară
    if d < TOO_CLOSE_DIST {
        println!("  [TACTIC] hero={} PREA APROAPE (dist={}) → retragere", hero.id, d);
        let retreats = kite_positions(hero, nearest_enemy, walls, map_w, map_h, false);
        if let Some(&(rx, ry)) = retreats.first() {
            return TacticalAction::Move { dest_x: rx, dest_y: ry, comment: "🏃retreat" };
        }
        return TacticalAction::Move { dest_x: hero.x, dest_y: hero.y, comment: "🛑stuck" };
    }

    // 2. Pot trage → trag imediat, fără nicio condiție suplimentară
    if hero.cooldown == 0 && i_have_los {
        println!("  [TACTIC] hero={} TRAG (dist={}, cooldown=0)", hero.id, d);
        return TacticalAction::Shoot {
            target_x: nearest_enemy.x,
            target_y: nearest_enemy.y,
            comment: "🔫",
        };
    }

    // 3. Pe cooldown și inamicul amenință → kiting lateral cu LoS păstrat
    if enemy_threatens {
        println!("  [TACTIC] hero={} KITING (dist={}, cooldown={}, enemy threatens)", hero.id, d, hero.cooldown);
        let kites = kite_positions(hero, nearest_enemy, walls, map_w, map_h, true);
        if let Some(&(kx, ky)) = kites.first() {
            return TacticalAction::Move { dest_x: kx, dest_y: ky, comment: "↔️kite" };
        }
        let kites_no_los = kite_positions(hero, nearest_enemy, walls, map_w, map_h, false);
        if let Some(&(kx, ky)) = kites_no_los.first() {
            return TacticalAction::Move { dest_x: kx, dest_y: ky, comment: "↔️kite-nlos" };
        }
    }

    // 4. Pe cooldown, inamic nu amenință → menține distanța preferată
    if hero.cooldown > 0 {
        if d > PREFERRED_KITE_DIST {
            println!("  [TACTIC] hero={} AVANSEZ (dist={} > {})", hero.id, d, PREFERRED_KITE_DIST);
            let (nx, ny) = bfs_next_step(hero.x, hero.y, nearest_enemy.x, nearest_enemy.y, walls, map_w, map_h);
            return TacticalAction::Move { dest_x: nx, dest_y: ny, comment: "→aproach" };
        } else {
            println!("  [TACTIC] hero={} STRAFE (dist={})", hero.id, d);
            let kites = kite_positions(hero, nearest_enemy, walls, map_w, map_h, true);
            if let Some(&(kx, ky)) = kites.first() {
                return TacticalAction::Move { dest_x: kx, dest_y: ky, comment: "↔️strafe" };
            }
            return TacticalAction::Move { dest_x: hero.x, dest_y: hero.y, comment: "🎯hold" };
        }
    }

    // 5. Fără LoS → avansăm BFS spre inamic
    println!("  [TACTIC] hero={} NO-LOS → BFS spre inamic", hero.id);
    let (nx, ny) = bfs_next_step(hero.x, hero.y, nearest_enemy.x, nearest_enemy.y, walls, map_w, map_h);
    TacticalAction::Move { dest_x: nx, dest_y: ny, comment: "🔍hunt" }
}

// ─────────────────────────────────────────────────────────────────────────────
// HELPERS PENTRU POZITII
// ─────────────────────────────────────────────────────────────────────────────

fn find_bottom_target(spawn_x: i32, map_h: i32, walls: &[Wall], map_w: i32) -> (i32, i32) {
    let mut y = map_h - 2;
    while y >= 1 {
        let (sx, sy) = snap_to_grid(spawn_x, y);
        if in_bounds(sx, sy, map_w, map_h) && !overlaps_wall(sx, sy, walls) {
            return (sx, sy);
        }
        y -= 3;
    }
    snap_to_grid(spawn_x, map_h / 2)
}

fn find_top_target(spawn_x: i32, map_h: i32, walls: &[Wall], map_w: i32) -> (i32, i32) {
    let mut y = 1;
    while y < map_h - 1 {
        let (sx, sy) = snap_to_grid(spawn_x, y);
        if in_bounds(sx, sy, map_w, map_h) && !overlaps_wall(sx, sy, walls) {
            return (sx, sy);
        }
        y += 3;
    }
    snap_to_grid(spawn_x, map_h / 2)
}

fn find_rally_point(map_w: i32, map_h: i32, walls: &[Wall], spawn_x: i32, spawn_y: i32) -> (i32, i32) {
    let (base_x, base_y) = snap_to_grid(spawn_x, spawn_y);
    let offsets: [i32; 9] = [0, 3, -3, 6, -6, 9, -9, 12, -12];
    for &dy in &offsets {
        for &dx in &offsets {
            let (sx, sy) = snap_to_grid(base_x + dx, base_y + dy);
            if in_bounds(sx, sy, map_w, map_h) && !overlaps_wall(sx, sy, walls) {
                return (sx, sy);
            }
        }
    }
    (base_x, base_y)
}

fn all_heroes_near(heroes: &[&Hero], tx: i32, ty: i32, threshold: i32) -> bool {
    heroes.iter().all(|h| (h.x - tx).abs() <= threshold && (h.y - ty).abs() <= threshold)
}

// ─────────────────────────────────────────────────────────────────────────────
// PROCESAREA TUREI
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn process_turn<S>(
    write: &mut S,
    my_player_id: i32,
    config: &GameConfig,
    map_walls: &[Wall],
    phase: &mut Phase,
    known_enemy_ids: &mut Vec<i32>,
    hunt_target_id: &mut Option<i32>,
    focus_target_id: &mut Option<i32>,
    rally_x: i32,
    rally_y: i32,
    _attack_x: i32,
    _attack_y: i32,
    turn_args: &StartTurnArgs,
) -> anyhow::Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let state = &turn_args.state;
    let map_w = config.width;
    let map_h = config.height;

    let my_heroes: Vec<&Hero> = state.heroes.iter()
        .filter(|h| h.owner_id == my_player_id)
        .collect();

    let enemy_heroes: Vec<&Hero> = state.heroes.iter()
        .filter(|h| h.owner_id != my_player_id)
        .collect();

    // Actualizăm lista de inamici văzuți vreodată
    for e in &enemy_heroes {
        if !known_enemy_ids.contains(&e.id) {
            known_enemy_ids.push(e.id);
            println!("  [INFO] Inamic nou descoperit: id={}", e.id);
        }
    }

    // ── Focus fire ────────────────────────────────────────────────────────
    {
        let alive_ids: Vec<i32> = enemy_heroes.iter().map(|e| e.id).collect();
        if let Some(fid) = *focus_target_id {
            if !alive_ids.contains(&fid) {
                println!("  [FOCUS] Ținta {} a murit, căutăm alta...", fid);
                *focus_target_id = None;
            }
        }
        if focus_target_id.is_none() {
            'outer: for hero in &my_heroes {
                for enemy in &enemy_heroes {
                    if has_line_of_sight(hero.x, hero.y, enemy.x, enemy.y, map_walls) {
                        *focus_target_id = Some(enemy.id);
                        println!("  [FOCUS] Țintă comună nouă: inamic id={}", enemy.id);
                        break 'outer;
                    }
                }
            }
        }
        println!("  [FOCUS] Țintă curentă: {:?}", focus_target_id);
    }

    // ── Tranziții de fază ─────────────────────────────────────────────────
    // Endgame override: intră indiferent de faza curentă
    if turn_args.turn >= ENDGAME_TURN && *phase != Phase::Endgame {
        println!("  [FAZA] → Endgame: tura {} >= {}, mergem la centru!", turn_args.turn, ENDGAME_TURN);
        *phase = Phase::Endgame;
    }

    match phase {
        Phase::Rally => {
            if all_heroes_near(&my_heroes, rally_x, rally_y, 9) {
                println!("  [FAZA] Rally → Attack: grupul s-a adunat!");
                *phase = Phase::Attack;
            }
        }
        Phase::Attack => {
            let alive_ids: Vec<i32> = enemy_heroes.iter().map(|e| e.id).collect();
            let first_kill = known_enemy_ids.iter().find(|id| !alive_ids.contains(id));
            if let Some(killed_id) = first_kill {
                println!("  [FAZA] Attack → Hunt: inamicul {} a murit!", killed_id);
                *hunt_target_id = alive_ids.first().copied();
                println!("  [FAZA] Vânăm inamicul: {:?}", hunt_target_id);
                *phase = Phase::Hunt;
            }
        }
        Phase::Hunt => {
            if let Some(tid) = *hunt_target_id {
                let alive_ids: Vec<i32> = enemy_heroes.iter().map(|e| e.id).collect();
                if !alive_ids.contains(&tid) {
                    *hunt_target_id = alive_ids.first().copied();
                    println!("  [HUNT] Ținta a murit, nou target: {:?}", hunt_target_id);
                }
            }
        }
        Phase::Endgame => {} // nicio tranziție, rămânem până la final
    }

    println!("  [FAZA CURENTA] {:?} | inamici vizibili: {}", phase, enemy_heroes.len());

    // ── Construim mesajele pentru fiecare erou ─────────────────────────────

    let mut messages: Vec<Message> = Vec::new();

    for hero in &my_heroes {

        // ── Destinația de mișcare din logica de fază (fallback dacă nu există inamici vizibili)
        let (center_x, center_y) = snap_to_grid(map_w / 2, map_h / 2);
        // În Endgame destinația de fallback (când nu e inamic vizibil) e centrul hărții.
        // Când există inamic vizibil, choose_tactical_action trage mai întâi dacă poate,
        // și abia dacă trebuie să se miște folosește această destinație pentru kiting.
        let phase_dest = match phase {
            Phase::Rally => (rally_x, rally_y),
            Phase::Endgame => (center_x, center_y),
            Phase::Attack | Phase::Hunt => {
                let cycle = turn_args.turn % 15;
                if cycle < 5 {
                    (center_x, center_y)
                } else if cycle < 10 {
                    let (lx, ly) = snap_to_grid(1, hero.y);
                    (lx, ly)
                } else {
                    (rally_x, rally_y)
                }
            }
        };

        // ── În faza Rally: mergi direct la rally point, fără simulare tactică ──
        if *phase == Phase::Rally {
            let (mx, my) = bfs_next_step(hero.x, hero.y, rally_x, rally_y, map_walls, map_w, map_h);
            let json = serde_json::json!({
                "command": "MOVE",
                "args": { "hero_id": hero.id, "x": mx, "y": my, "comment": "🏃rally" }
            });
            messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
            continue;
        }

        // ── Găsim cel mai apropiat inamic vizibil (pentru focus fire sau tactic) ──
        // Prioritatea 1: focus_target_id dacă îl vedem
        // Prioritatea 2: orice inamic cu LoS
        // Prioritatea 3: cel mai aproape inamic (chiar fără LoS) → pentru mișcare
        let visible_focus = focus_target_id.and_then(|fid| {
            enemy_heroes.iter().find(|e| {
                e.id == fid && has_line_of_sight(hero.x, hero.y, e.x, e.y, map_walls)
            }).copied()
        });

        let nearest_enemy: Option<&&Hero> = if visible_focus.is_some() {
            enemy_heroes.iter().find(|e| Some(e.id) == *focus_target_id)
        } else {
            // Cel mai aproape inamic (cu sau fără LoS) — pentru mișcare/simulare
            enemy_heroes.iter().min_by_key(|e| {
                let dx = hero.x - e.x;
                let dy = hero.y - e.y;
                dx * dx + dy * dy
            })
        };

        match nearest_enemy {
            None => {
                // Niciun inamic vizibil → mișcă spre destinația de fază
                let (mx, my) = bfs_next_step(hero.x, hero.y, phase_dest.0, phase_dest.1, map_walls, map_w, map_h);
                let json = serde_json::json!({
                    "command": "MOVE",
                    "args": { "hero_id": hero.id, "x": mx, "y": my, "comment": "🏃noenemies" }
                });
                messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
            }
            Some(enemy) => {
                // ── Alegem acțiunea tactică optimă ──────────────────────────────
                let action = choose_tactical_action(
                    hero,
                    enemy,
                    map_walls,
                    map_w,
                    map_h,
                    phase_dest,
                );

                // ── Construim mesajul corespunzător acțiunii alese ───────────────
                let json = match action {
                    TacticalAction::Shoot { target_x, target_y, comment } => {
                        serde_json::json!({
                            "command": "SHOOT",
                            "args": {
                                "hero_id": hero.id,
                                "x": target_x,
                                "y": target_y,
                                "comment": comment
                            }
                        })
                    }
                    TacticalAction::Move { dest_x, dest_y, comment } => {
                        // Aplicăm BFS pentru a obține pasul concret
                        let (mx, my) = bfs_next_step(
                            hero.x, hero.y,
                            dest_x, dest_y,
                            map_walls, map_w, map_h,
                        );
                        serde_json::json!({
                            "command": "MOVE",
                            "args": {
                                "hero_id": hero.id,
                                "x": mx,
                                "y": my,
                                "comment": comment
                            }
                        })
                    }
                };

                messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
            }
        }
    }

    println!("  [SEND_ALL] {} mesaje", messages.len());
    write.send_all(&mut futures_util::stream::iter(messages).map(Ok)).await
        .context("eroare send_all")?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// MAIN
// ─────────────────────────────────────────────────────────────────────────────

pub const VERSUS_PLAYERS: bool = false;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = "wss://bitdefenders.cvjd.me/ws";
    println!("Conectare la {url} ...");

    let (ws, _) = connect_async(url).await.context("nu s-a putut conecta")?;
    let (mut write, mut read) = ws.split();
    println!("Conectat!");

    let mut config: Option<GameConfig> = None;
    let mut my_player_id: i32 = 0;

    let mut phase = Phase::Rally;
    let mut known_enemy_ids: Vec<i32> = Vec::new();
    let mut hunt_target_id: Option<i32> = None;
    let mut focus_target_id: Option<i32> = None;

    let mut rally_x: i32 = 0;
    let mut rally_y: i32 = 0;
    let mut attack_x: i32 = 0;
    let mut attack_y: i32 = 0;

    let mut map_walls: Vec<Wall> = Vec::new();

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => { println!("Eroare WebSocket: {e:?}"); break; }
        };

        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(payload) => { write.send(Message::Pong(payload)).await?; continue; }
            Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => continue,
            Message::Close(frame) => { println!("Conexiune închisă: {frame:?}"); break; }
        };

        let msg: WsMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => { println!("Parse error: {e}\nRaw: {text}"); continue; }
        };

        println!("[SERVER] → {}", msg.command);

        match msg.command.as_str() {
            "HELLO" => {
                send_msg(&mut write, "LOGIN", serde_json::json!({
                    "name": "Damoc_Damian",
                    "version": PROTOCOL_VERSION
                })).await?;
            }
            "READY" => {
                if VERSUS_PLAYERS {
                    send_msg(&mut write, "CHALLENGE", serde_json::json!({})).await?;
                } else {
                    send_msg(&mut write, "PRACTICE", serde_json::json!({})).await?;
                }
            }
            "START_MATCH" => {
                let args: StartMatchArgs = serde_json::from_value(msg.args)
                    .context("eroare la parsarea START_MATCH")?;

                println!("Meci pornit! ID={} player_id={} hartă={}x{}",
                    args.match_id, args.your_player_id,
                    args.config.width, args.config.height);

                map_walls = args.state.walls;
                println!("  ziduri pe hartă: {}", map_walls.len());

                my_player_id = args.your_player_id;

                let map_w = args.config.width;
                let map_h = args.config.height;

                let my_spawn_x = args.config.players.iter()
                    .find(|p| p.id == my_player_id)
                    .and_then(|p| p.heroes.first())
                    .map(|h| h.x)
                    .unwrap_or(map_w / 2);
                let my_spawn_y = args.config.players.iter()
                    .find(|p| p.id == my_player_id)
                    .and_then(|p| p.heroes.first())
                    .map(|h| h.y)
                    .unwrap_or(0);
                let enemy_spawn_x = args.config.players.iter()
                    .find(|p| p.id != my_player_id)
                    .and_then(|p| p.heroes.first())
                    .map(|h| h.x)
                    .unwrap_or(map_w / 2);

                let we_are_at_bottom = my_spawn_y > map_h / 2;
                let rally_tile_y = if we_are_at_bottom { map_h - 20 } else { 19 };

                let (rx, ry) = find_rally_point(map_w, map_h, &map_walls, 10, rally_tile_y);
                rally_x = rx;
                rally_y = ry;

                let (ax, ay) = if we_are_at_bottom {
                    find_top_target(enemy_spawn_x, map_h, &map_walls, map_w)
                } else {
                    find_bottom_target(enemy_spawn_x, map_h, &map_walls, map_w)
                };
                attack_x = ax;
                attack_y = ay;

                println!("  [INIT] rally=({},{}) attack=({},{})", rally_x, rally_y, attack_x, attack_y);

                config = Some(args.config);

                phase = Phase::Rally;
                known_enemy_ids = Vec::new();
                hunt_target_id = None;
                focus_target_id = None;
            }
            "START_TURN" => {
                let args: StartTurnArgs = serde_json::from_value(msg.args)
                    .context("eroare la parsarea START_TURN")?;

                if let Some(cfg) = &config {
                    if let Err(e) = process_turn(
                        &mut write,
                        my_player_id,
                        cfg,
                        &map_walls,
                        &mut phase,
                        &mut known_enemy_ids,
                        &mut hunt_target_id,
                        &mut focus_target_id,
                        rally_x,
                        rally_y,
                        attack_x,
                        attack_y,
                        &args,
                    ).await {
                        println!("Eroare în process_turn: {e}");
                    }
                }
            }
            "END_MATCH" => {
                let args: EndMatchArgs = serde_json::from_value(msg.args)
                    .context("eroare la parsarea END_MATCH")?;
                match &args.winner {
                    Some(w) => println!("Câștigător: {w} (motiv: {})", args.reason),
                    None    => println!("Egalitate (motiv: {})", args.reason),
                }
                break;
            }
            "ERROR" => {
                let fatal = msg.args["fatal"].as_bool().unwrap_or(false);
                println!("EROARE server: {} (fatal={fatal})", msg.args["message"]);
                if fatal { break; }
            }
            other => println!("Comandă necunoscută: {other}"),
        }
    }

    println!("Deconectat.");
    Ok(())
}