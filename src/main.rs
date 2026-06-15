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

/// Fazele strategice ale botului.
/// Rally   → toți eroii merg la punctul de raliere din dreapta hărții
/// Attack  → grupul merge împreună spre spawn-ul inamicilor
/// Hunt    → primul inamic a murit; căutăm al doilea pe toată harta
/// Pincer  → rămâne exact 1 inamic și ≥2 eroi proprii; fiecare flancher atacă din altă parte
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Rally,
    Attack,
    Hunt,
    Pincer,
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

/// Rally point fix: tile (10, 19).
/// Dacă e blocat de zid, caută cea mai apropiată celulă liberă în jur.
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

/// Verifică dacă toți eroii proprii sunt la o distanță ≤ threshold față de (tx, ty).
/// Folosit pentru a detecta când grupul s-a adunat la rally point.
fn all_heroes_near(heroes: &[&Hero], tx: i32, ty: i32, threshold: i32) -> bool {
    heroes.iter().all(|h| (h.x - tx).abs() <= threshold && (h.y - ty).abs() <= threshold)
}

// ─────────────────────────────────────────────────────────────────────────────
// DODGE LOGIC  ← NOU
// ─────────────────────────────────────────────────────────────────────────────

/// Calculează o poziție de evaziune perpendiculară față de direcția atacantului.
/// `shooter_x/y` → poziția inamicului care trage
/// `hero_x/y`    → poziția eroului nostru
/// Returnează cel mai bun tile liber perpendicular pe axa de atac.
fn dodge_position(
    hero_x: i32, hero_y: i32,
    shooter_x: i32, shooter_y: i32,
    walls: &[Wall],
    map_w: i32, map_h: i32,
) -> (i32, i32) {
    // Vectorul de atac (de la shooter spre erou)
    let dx = hero_x - shooter_x;
    let dy = hero_y - shooter_y;

    // Perpendiculare pe axa de atac: doi pași (6 unități) ca primă opțiune,
    // un pas (3 unități) ca fallback dacă cei doi sunt blocați de zid
    let perp_candidates: [(i32, i32); 4] = [
        ( dy.signum() * 6, -dx.signum() * 6),  // doi pași lateral dreapta
        (-dy.signum() * 6,  dx.signum() * 6),  // doi pași lateral stânga
        ( dy.signum() * 3, -dx.signum() * 3),  // fallback: un pas lateral dreapta
        (-dy.signum() * 3,  dx.signum() * 3),  // fallback: un pas lateral stânga
    ];

    for (pdx, pdy) in perp_candidates {
        let (nx, ny) = snap_to_grid(hero_x + pdx, hero_y + pdy);
        if in_bounds(nx, ny, map_w, map_h) && !overlaps_wall(nx, ny, walls) {
            return (nx, ny);
        }
    }

    // Dacă nu găsim lateral, facem doi pași înapoi față de shooter
    let back_x = hero_x + dx.signum() * 6;
    let back_y = hero_y + dy.signum() * 6;
    let (bx, by) = snap_to_grid(back_x, back_y);
    if in_bounds(bx, by, map_w, map_h) && !overlaps_wall(bx, by, walls) {
        return (bx, by);
    }

    (hero_x, hero_y) // stăm pe loc dacă nu există nicio ieșire
}

/// Determină dacă un erou propriu este în pericol iminent de atac.
/// Un inamic este "periculos" dacă:
///   1. Are linie de vedere spre eroul nostru
///   2. Cooldown-ul lui este ≤ 1 (trage în această tură sau în următoarea)
///      SAU a tras recent și ciclul e aproape complet (trage din nou în curând)
fn is_under_attack_threat(
    hero_x: i32, hero_y: i32,
    enemy: &Hero,
    enemy_last_shot: &HashMap<i32, i32>,
    current_turn: i32,
    shoot_cooldown: i32,
    walls: &[Wall],
) -> bool {
    if !has_line_of_sight(hero_x, hero_y, enemy.x, enemy.y, walls) {
        return false;
    }

    // Inamicul trage din nou în curând: cooldown ≤ 2 (anticipăm cu 2 ture)
    if enemy.cooldown <= 2 {
        return true;
    }

    // Inamicul a tras recent și intervalul e aproape complet → va trage curând
    // Pragul e - 3 față de shoot_cooldown pentru a anticipa cu o tură în plus
    if let Some(&last_turn) = enemy_last_shot.get(&enemy.id) {
        let turns_since_shot = current_turn - last_turn;
        if turns_since_shot >= shoot_cooldown - 3 {
            return true;
        }
    }

    false
}

// ─────────────────────────────────────────────────────────────────────────────
// PINCER / FLANCARE
// ─────────────────────────────────────────────────────────────────────────────

/// Calculează două poziții de flancare în jurul inamicului, la distanța `dist`.
/// Eroii se vor poziționa perpendicular față de centrul hărții, câte unul pe
/// fiecare parte, astfel încât inamicul să fie prins în crossfire.
/// Returnează (flank_left, flank_right) — fiecare erou ia câte una.
fn flank_positions(
    enemy_x: i32, enemy_y: i32,
    map_w: i32, map_h: i32,
    walls: &[Wall],
) -> ((i32, i32), (i32, i32)) {
    // Perpendiculara față de axa centru→inamic
    let cx = map_w / 2;
    let cy = map_h / 2;
    let dx = enemy_x - cx;
    let dy = enemy_y - cy;

    // Perpendiculara unitară (normalizată la 9 — 3 pași de grid)
    let dist = 9i32;
    let (px, py) = if dx == 0 && dy == 0 {
        (dist, 0)
    } else {
        // Rotim 90°: (-dy, dx), scalat la `dist`
        let len = (((dx * dx + dy * dy) as f64).sqrt()).max(1.0);
        let npx = ((-dy as f64 / len) * dist as f64).round() as i32;
        let npy = ((dx as f64 / len) * dist as f64).round() as i32;
        // snap la multiplu de 3
        let snap3 = |v: i32| -> i32 {
            let r = v % 3;
            if r == 0 { v } else if r.abs() == 1 { v - r } else { v + (3 - r.abs()) * r.signum() }
        };
        (snap3(npx).max(-dist).min(dist), snap3(npy).max(-dist).min(dist))
    };

    // Candidați: 9, 6, 3 unități pe fiecare parte, până găsim tile liber
    for scale in [1i32, 2, 3] {
        let (l1x, l1y) = snap_to_grid(enemy_x + px * scale / 1, enemy_y + py * scale / 1);
        let (l2x, l2y) = snap_to_grid(enemy_x - px * scale / 1, enemy_y - py * scale / 1);
        let left_ok  = in_bounds(l1x, l1y, map_w, map_h) && !overlaps_wall(l1x, l1y, walls);
        let right_ok = in_bounds(l2x, l2y, map_w, map_h) && !overlaps_wall(l2x, l2y, walls);
        if left_ok && right_ok {
            return ((l1x, l1y), (l2x, l2y));
        }
    }

    // Fallback: amândoi merg direct la inamic
    let (ex, ey) = snap_to_grid(enemy_x, enemy_y);
    ((ex, ey), (ex, ey))
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
    enemy_last_shot: &mut HashMap<i32, i32>,        // NOU: tura în care fiecare inamic a tras ultima oară
    prev_enemy_cooldowns: &mut HashMap<i32, i32>,   // NOU: cooldown-urile inamicilor din tura precedentă
    rally_x: i32,
    rally_y: i32,
    attack_x: i32,
    attack_y: i32,
    turn_args: &StartTurnArgs,
) -> anyhow::Result<()>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let state = &turn_args.state;
    let map_w = config.width;
    let map_h = config.height;
    let current_turn = turn_args.turn;

    let my_heroes: Vec<&Hero> = state.heroes.iter()
        .filter(|h| h.owner_id == my_player_id)
        .collect();

    let enemy_heroes: Vec<&Hero> = state.heroes.iter()
        .filter(|h| h.owner_id != my_player_id)
        .collect();

    // ── NOU: Detectăm când inamicii au tras ───────────────────────────────
    // Un inamic a tras în tura precedentă dacă cooldown-ul era 0 și acum s-a resetat.
    for enemy in &enemy_heroes {
        let prev_cd = prev_enemy_cooldowns.get(&enemy.id).copied().unwrap_or(-1);
        if prev_cd == 0 && enemy.cooldown > 0 {
            enemy_last_shot.insert(enemy.id, current_turn - 1);
            println!("  [DODGE] Inamic {} a tras la tura {}", enemy.id, current_turn - 1);
        }
        prev_enemy_cooldowns.insert(enemy.id, enemy.cooldown);
    }

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
            // Dacă rămâne exact 1 inamic și avem ≥2 eroi → Pincer
            if enemy_heroes.len() == 1 && my_heroes.len() >= 2 {
                println!("  [FAZA] Attack → Pincer: 1 inamic, {} eroi proprii!", my_heroes.len());
                *phase = Phase::Pincer;
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
            // Dacă rămâne exact 1 inamic și avem ≥2 eroi → Pincer
            if enemy_heroes.len() == 1 && my_heroes.len() >= 2 {
                println!("  [FAZA] Hunt → Pincer: 1 inamic, {} eroi proprii!", my_heroes.len());
                *phase = Phase::Pincer;
            }
        }
        Phase::Pincer => {
            // Ieșim din Pincer dacă inamicul a murit sau au mai apărut inamici
            if enemy_heroes.is_empty() || enemy_heroes.len() > 1 || my_heroes.len() < 2 {
                println!("  [FAZA] Pincer → Hunt: condiție pincer nu mai e îndeplinită");
                *hunt_target_id = enemy_heroes.iter().map(|e| e.id).next();
                *phase = Phase::Hunt;
            }
        }
    }

    println!("  [FAZA CURENTA] {:?} | inamici vizibili: {}", phase, enemy_heroes.len());

    // ── NOU: Shoot cooldown al inamicilor din config ───────────────────────
    // Folosit pentru a estima când trag din nou după ultimul shot detectat.
    let enemy_shoot_cooldown = config.hero_types.values()
        .map(|t| t.shoot_cooldown)
        .next()
        .unwrap_or(5);

    // ── Construim mesajele pentru fiecare erou ────────────────────────────

    let mut messages: Vec<Message> = Vec::new();

    for hero in &my_heroes {

        // ── Tragere (prioritate maximă, indiferent de fază) ────────────────
        // Dacă eroul poate trage și are linie de vedere la țintă, trage și gata —
        // nu mai verificăm dodge în aceeași tură.
        let can_shoot = hero.cooldown == 0
            && !enemy_heroes.is_empty()
            && focus_target_id.map_or(false, |fid| {
                enemy_heroes.iter().find(|e| e.id == fid)
                    .map_or(false, |enemy| has_line_of_sight(hero.x, hero.y, enemy.x, enemy.y, map_walls))
            });

        if can_shoot {
            if let Some(fid) = *focus_target_id {
                if let Some(enemy) = enemy_heroes.iter().find(|e| e.id == fid) {
                    let json = serde_json::json!({
                        "command": "SHOOT",
                        "args": {
                            "hero_id": hero.id,
                            "x": enemy.x,
                            "y": enemy.y,
                            "comment": "🔫"
                        }
                    });
                    messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));

                    // După tragere, verificăm dacă trebuie să ne și ferim în aceeași tură.
                    // Inamicul poate trage simultan cu noi, deci ne mișcăm imediat după SHOOT.
                    let dodge_after_shoot = enemy_heroes.iter().find(|e| {
                        is_under_attack_threat(
                            hero.x, hero.y,
                            e,
                            enemy_last_shot,
                            current_turn,
                            enemy_shoot_cooldown,
                            map_walls,
                        )
                    });
                    if let Some(shooter) = dodge_after_shoot {
                        let (dodge_x, dodge_y) = dodge_position(
                            hero.x, hero.y,
                            shooter.x, shooter.y,
                            map_walls,
                            map_w, map_h,
                        );
                        if dodge_x != hero.x || dodge_y != hero.y {
                            println!(
                                "  [DODGE+SHOOT] Erou {} trage și se ferește de inamic {} → ({},{})",
                                hero.id, shooter.id, dodge_x, dodge_y
                            );
                            let json = serde_json::json!({
                                "command": "MOVE",
                                "args": {
                                    "hero_id": hero.id,
                                    "x": dodge_x,
                                    "y": dodge_y,
                                    "comment": "🔫💨"
                                }
                            });
                            messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
                        }
                    }
                    continue; // skip restul logicii de mișcare
                }
            }
        }

        // ── Dodge — verificăm dacă vreun inamic e pe cale să tragă ──────────
        // Ajungem aici DOAR dacă nu am putut trage în această tură.
        let dodge_threat = enemy_heroes.iter().find(|enemy| {
            is_under_attack_threat(
                hero.x, hero.y,
                enemy,
                enemy_last_shot,
                current_turn,
                enemy_shoot_cooldown,
                map_walls,
            )
        });

        if let Some(shooter) = dodge_threat {
            let (dodge_x, dodge_y) = dodge_position(
                hero.x, hero.y,
                shooter.x, shooter.y,
                map_walls,
                map_w, map_h,
            );

            // Facem dodge doar dacă chiar ne mișcăm (nu stăm pe loc inutil)
            if dodge_x != hero.x || dodge_y != hero.y {
                println!(
                    "  [DODGE] Erou {} se ferește de inamic {} → ({},{})",
                    hero.id, shooter.id, dodge_x, dodge_y
                );
                let json = serde_json::json!({
                    "command": "MOVE",
                    "args": {
                        "hero_id": hero.id,
                        "x": dodge_x,
                        "y": dodge_y,
                        "comment": "💨"
                    }
                });
                messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
                continue; // skip mișcarea normală
            }
        }

        // ── Mișcare în funcție de fază ─────────────────────────────────────
        let any_enemy_visible = enemy_heroes.iter()
            .any(|e| has_line_of_sight(hero.x, hero.y, e.x, e.y, map_walls));

        let (center_x, center_y) = snap_to_grid(map_w / 2, map_h / 2);

        let (dest_x, dest_y) = match phase {
            Phase::Rally => (rally_x, rally_y),
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
            Phase::Pincer => {
                // Fiecare erou merge la o poziție de flancare diferită.
                // Eroii sunt sortați după id pentru a li se atribui consistent
                // flank_left vs flank_right fără a depinde de ordinea din vector.
                if let Some(enemy) = enemy_heroes.first() {
                    let (flank_left, flank_right) = flank_positions(
                        enemy.x, enemy.y,
                        map_w, map_h,
                        map_walls,
                    );
                    // Cel cu id-ul mai mic → flank_left, cel cu id mai mare → flank_right
                    let min_hero_id = my_heroes.iter().map(|h| h.id).min().unwrap_or(hero.id);
                    if hero.id == min_hero_id {
                        println!("  [PINCER] Erou {} → flank_left ({},{})", hero.id, flank_left.0, flank_left.1);
                        flank_left
                    } else {
                        println!("  [PINCER] Erou {} → flank_right ({},{})", hero.id, flank_right.0, flank_right.1);
                        flank_right
                    }
                } else {
                    (rally_x, rally_y)
                }
            }
        };

        let (move_x, move_y) = bfs_next_step(
            hero.x, hero.y,
            dest_x, dest_y,
            map_walls,
            map_w, map_h,
        );

        let comment = match phase {
            Phase::Rally => "🏃",
            Phase::Pincer => "🔀",
            Phase::Attack | Phase::Hunt => {
                if any_enemy_visible {
                    let cycle = turn_args.turn % 15;
                    if cycle < 5 { "🎯→" } else if cycle < 10 { "←🔫" } else { "↩️🔫" }
                } else {
                    "↩️"
                }
            }
        };

        let json = serde_json::json!({
            "command": "MOVE",
            "args": {
                "hero_id": hero.id,
                "x": move_x,
                "y": move_y,
                "comment": comment
            }
        });
        messages.push(Message::Text(serde_json::to_string(&json).unwrap().into()));
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

    // Starea strategică
    let mut phase = Phase::Rally;
    let mut known_enemy_ids: Vec<i32> = Vec::new();
    let mut hunt_target_id: Option<i32> = None;
    let mut focus_target_id: Option<i32> = None;
    let mut enemy_last_shot: HashMap<i32, i32> = HashMap::new();        // NOU
    let mut prev_enemy_cooldowns: HashMap<i32, i32> = HashMap::new();   // NOU

    // Puncte cheie pe hartă
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

                // Spawn-ul nostru și al inamicului
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

                let we_are_at_bottom = my_spawn_y > map_h/2;
                let rally_tile_y = if we_are_at_bottom { map_h - 20 } else { 19 };

                let (rx, ry) = find_rally_point(map_w, map_h, &map_walls, 10, rally_tile_y);
                rally_x = rx;
                rally_y = ry;

                let we_are_at_bottom = my_spawn_y > map_h / 2;
                let (ax, ay) = if we_are_at_bottom {
                    find_top_target(enemy_spawn_x, map_h, &map_walls, map_w)
                } else {
                    find_bottom_target(enemy_spawn_x, map_h, &map_walls, map_w)
                };
                attack_x = ax;
                attack_y = ay;

                println!("  [INIT] rally=({},{}) attack=({},{})", rally_x, rally_y, attack_x, attack_y);

                config = Some(args.config);

                // Reset stare la începutul meciului
                phase = Phase::Rally;
                known_enemy_ids = Vec::new();
                hunt_target_id = None;
                focus_target_id = None;
                enemy_last_shot = HashMap::new();       // NOU
                prev_enemy_cooldowns = HashMap::new();  // NOU
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
                        &mut enemy_last_shot,           // NOU
                        &mut prev_enemy_cooldowns,      // NOU
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