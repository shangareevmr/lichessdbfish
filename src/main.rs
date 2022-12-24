extern crate reqwest;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate rand;

use std::io::{self, BufRead, Write};
use std::process;
use std::iter;
use std::collections::HashMap;
use std::rc::Rc;
use std::borrow::Borrow;
use std::convert::TryFrom;

const STARTPOS: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

const UCI_CMD: &str       = "uci";
const SETOPTION_CMD: &str = "setoption";
const ISREADY_CMD: &str   = "isready";
const POSITION_CMD: &str  = "position";
const GO_CMD: &str        = "go";
const STOP_CMD: &str      = "stop";
const QUIT_CMD: &str      = "quit";
const EXIT_CMD: &str      = "exit";

const UCIOK_RESP: &str    = "uciok";
const OPTION_RESP: &str   = "option";
const READYOK_RESP: &str  = "readyok";
const INFO_RESP: &str     = "info";
const BESTMOVE_RESP: &str = "bestmove";

const NAME_PARAM: &str    = "name";
const VALUE_PARAM: &str   = "value";

struct Engine {
    master_games: bool,
    ratings: RatingFilter,
    tc: TimeControlFilter,

    opt_games_min: u64,
    opt_games_pct_min: u64,
    opt_score_pct_min: u64,
    opt_sortby: SortBy,
    opt_variants: usize,
    opt_weightby: WeightBy,

    fen: String,
    turn: Turn,
    cache: HashMap<String, Rc<PositionInfo>>,
}

struct RatingFilter {
    rating_1600: bool,
    rating_1800: bool,
    rating_2000: bool,
    rating_2200: bool,
    rating_2500: bool,
}

struct TimeControlFilter {
    bullet: bool,
    blitz: bool,
    rapid: bool,
    classical: bool,
}

enum SortBy {
    Games,
    Score,
}

enum WeightBy {
    Games,
    Score,
    Random,
}

enum Turn {
    White,
    Black,
}

#[derive(Deserialize, Serialize)]
struct Move {
    uci: String,
    san: String,
    white: u64,
    draws: u64,
    black: u64,
}

#[derive(Deserialize, Serialize)]
struct PositionInfo {
    white: u64,
    draws: u64,
    black: u64,
    moves: Vec<Move>,
}

fn fix_castle(lichess_move: &String) -> String {
    match lichess_move.as_str() {
        "e1h1" => "e1g1",
        "e1a1" => "e1c1",
        "e8h8" => "e8g8",
        "e8a8" => "e8c8",
        _ => lichess_move.as_str()
    }.to_string()
}

fn get_position_info(fen: &str, master_games: bool, ratings: &RatingFilter, tc: &TimeControlFilter) -> Result<PositionInfo, String> {
    let mut params = vec![("fen", fen), ("moves", "50")];

    let url_str = if master_games {
        "https://explorer.lichess.ovh/master"
    } else {
        params.push(("variant", "standard"));
        if ratings.rating_1600 {
            params.push(("ratings[]", "1600"));
        }
        if ratings.rating_1800 {
            params.push(("ratings[]", "1800"));
        }
        if ratings.rating_2000 {
            params.push(("ratings[]", "2000"));
        }
        if ratings.rating_2200 {
            params.push(("ratings[]", "2200"));
        }
        if ratings.rating_2500 {
            params.push(("ratings[]", "2500"));
        }

        if tc.bullet {
            params.push(("speeds[]", "bullet"));
        }
        if tc.blitz {
            params.push(("speeds[]", "blitz"));
        }
        if tc.rapid {
            params.push(("speeds[]", "rapid"));
        }
        if tc.classical {
            params.push(("speeds[]", "classical"));
        }

        "https://explorer.lichess.ovh/lichess"
    };

    let url = match reqwest::Url::parse_with_params(url_str, &params) {
        Ok(url) => url,
        Err(err) => return Err(format!("Url parse error: {}", err))
    };
    let text = match reqwest::get(url) {
        Ok(mut resp) => match resp.text() {
            Ok(text) => text,
            Err(err) => return Err(format!("HTTP response body error: {}", err))
        },
        Err(err) => return Err(format!("HTTP request error: {}", err))
    };
    match serde_json::from_str(&text) {
        Ok(position) => Ok(position),
        Err(err) => return Err(format!("JSON parse error: {}", err))
    }
}

fn get_position_info_cached(fen: &str, master_games: bool, ratings: &RatingFilter, tc: &TimeControlFilter, cache: &mut HashMap<String, Rc<PositionInfo>>) -> Option<Rc<PositionInfo>> {
    match cache.get(fen) {
        Some(position) => Some(position.clone()),
        None => {
            let position = match get_position_info(fen, master_games, ratings, tc) {
                Ok(position) => Rc::new(position),
                Err(_) => {
                    return None
                }
            };
            let result = position.clone();
            cache.insert(fen.to_string(), position);
            Some(result)
        }
    }
}

fn get_position_move(engine: &Engine, position: &PositionInfo, mut rng: impl rand::Rng) -> Option<Move> {
    let mut moves: Vec<&Move> = position.moves.iter()
        .filter(|x| x.white + x.black + x.draws >= engine.opt_games_min)
        .filter(|x| (x.white + x.black + x.draws)*100/(position.white + position.draws + position.black) >= engine.opt_games_pct_min)
        .filter(|x| (2*(if let Turn::White = engine.turn { x.white } else { x.black }) + x.draws)*100/(2*x.white + x.black + 2*x.draws) >= engine.opt_score_pct_min)
        .collect();

    moves.sort_by(|x, y| {
        match &engine.opt_sortby {
            SortBy::Games => (y.white + y.black + y.draws).cmp(&(x.white + x.black + x.draws)),
            SortBy::Score => ((2*(if let Turn::White = engine.turn { y.white } else { y.black }) + y.draws)*100/(2*y.white + y.black + 2*y.draws)).cmp(&((2*(if let Turn::White = engine.turn { x.white } else { x.black }) + x.draws)*100/(2*x.white + x.black + 2*x.draws))),
        }
    });

    moves.truncate(engine.opt_variants);

    let total_weight = match engine.opt_weightby {
        WeightBy::Games => moves.iter().fold(0, |acc, x| acc + x.white + x.draws + x.black),
        WeightBy::Score => moves.iter().fold(0, |acc, x| acc + (2*(if let Turn::White = engine.turn { x.white } else { x.black }) + x.draws)*100/(2*x.white + x.black + 2*x.draws)),
        WeightBy::Random => u64::try_from(moves.len()).unwrap()
    };

    if total_weight != 0 {
        let random = rng.gen_range(0, total_weight);
        let mut acc_weight = 0;
        for x in moves {
            acc_weight += match engine.opt_weightby {
                WeightBy::Games => x.white + x.draws + x.black,
                WeightBy::Score => (2*(if let Turn::White = engine.turn { x.white } else { x.black }) + x.draws)*100/(2*x.white + x.black + 2*x.draws),
                WeightBy::Random => 1
            };

            if random < acc_weight {
                return Some(Move {
                    uci: fix_castle(&x.uci),
                    san: x.san.clone(),
                    white: x.white,
                    draws: x.draws,
                    black: x.black
                })
            }
        }
    }

    None
}

fn process_uci<I, L>(mut stockfish_stdin: I, mut stockfish_lines: L) -> bool where
    I: io::Write,
    L: iter::Iterator<Item = String>
{
    writeln!(stockfish_stdin, "{}", UCI_CMD).unwrap();

    loop {
        match stockfish_lines.next() {
            Some(line) => {
                if line.as_str().trim() == UCIOK_RESP {
                    println!("{} {} LichessDB_Masters type check default false", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Bullet type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Blitz type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rapid type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Classical type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rating_1600_1800 type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rating_1800_2000 type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rating_2000_2200 type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rating_2200_2500 type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Rating_Above_2500 type check default true", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Games_GT type spin default 30 min 1 max 1000000000", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Games_Percent_GT type spin default 1 min 0 max 100", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Score_GT type spin default 0 min 0 max 100", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Sort_By type combo default Games var Games var Score", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Variants type spin default 1 min 0 max 50", OPTION_RESP, NAME_PARAM);
                    println!("{} {} LichessDB_Variant_Weight type combo default Random var Games var Score var Random", OPTION_RESP, NAME_PARAM);
                    println!("{}", UCIOK_RESP);
                    break
                }
                println!("{}", line);
            }
            None => return false
        }
    }
    true
}

fn process_setoption<I>(engine: &mut Engine, args: Vec<&str>, mut stockfish_stdin: I) -> bool where
    I: io::Write,
{
    let mut arg = args.iter();
    match arg.next() {
        Some(&NAME_PARAM) => match arg.next() {
            Some(&"LichessDB_Variant_Weight") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"Games") => engine.opt_weightby = WeightBy::Games, 
                    Some(&"Score") => engine.opt_weightby = WeightBy::Score, 
                    Some(&"Random") => engine.opt_weightby = WeightBy::Random, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Games_GT") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(x) => match x.parse().unwrap() {
                        1...1000000000 => engine.opt_games_min = x.parse().unwrap(),
                        _ => (),
                    }
                    None => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Games_Percent_GT") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(x) => match x.parse().unwrap() {
                        1...100 => engine.opt_games_pct_min = x.parse().unwrap(),
                        _ => (),
                    }
                    None => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Score_GT") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(x) => match x.parse().unwrap() {
                        1...100 => engine.opt_score_pct_min = x.parse().unwrap(),
                        _ => (),
                    }
                    None => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Variants") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(x) => match x.parse().unwrap() {
                        0...50 => engine.opt_variants = x.parse().unwrap(),
                        _ => (),
                    }
                    None => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Sort_By") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"Games") => engine.opt_sortby = SortBy::Games, 
                    Some(&"Score") => engine.opt_sortby = SortBy::Score, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Masters") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.master_games = true, 
                    Some(&"false") => engine.master_games = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rating_1600_1800") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.ratings.rating_1600 = true, 
                    Some(&"false") => engine.ratings.rating_1600 = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rating_1800_2000") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.ratings.rating_1800 = true, 
                    Some(&"false") => engine.ratings.rating_1800 = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rating_2000_2200") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.ratings.rating_2000 = true, 
                    Some(&"false") => engine.ratings.rating_2000 = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rating_2200_2500") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.ratings.rating_2200 = true, 
                    Some(&"false") => engine.ratings.rating_2200 = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rating_Above_2500") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.ratings.rating_2500 = true, 
                    Some(&"false") => engine.ratings.rating_2500 = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Bullet") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.tc.bullet = true, 
                    Some(&"false") => engine.tc.bullet = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Blitz") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.tc.blitz = true, 
                    Some(&"false") => engine.tc.blitz = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Rapid") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.tc.rapid = true, 
                    Some(&"false") => engine.tc.rapid = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(&"LichessDB_Classical") => match arg.next() {
                Some(&VALUE_PARAM) => match arg.next() {
                    Some(&"true") => engine.tc.classical = true, 
                    Some(&"false") => engine.tc.classical = false, 
                    _ => (),
                }
                _ => (),
            }
            Some(_) => {
                let command_line = args.iter().fold(SETOPTION_CMD.to_string(), |acc, x| acc + " " + x);
                writeln!(stockfish_stdin, "{}", command_line).unwrap();
            }
            None => println!("No such option: "),
        }
        _ => println!("No such option: "),
    }
    true
}

fn process_isready<I, L>(mut stockfish_stdin: I, mut stockfish_lines: L) -> bool where
    I: io::Write,
    L: iter::Iterator<Item = String>
{
    writeln!(stockfish_stdin, "{}", ISREADY_CMD).unwrap();

    match stockfish_lines.next() {
        Some(line) => {
            if line.as_str().trim() == READYOK_RESP {
                println!("{}", line);
            } else {
                return false
            }
        }
        None => return false
    }
    true
}

fn process_position<I>(engine: &mut Engine, args: Vec<&str>, mut stockfish_stdin: I) -> bool where
    I: io::Write,
{
    if args.is_empty() {
        return true
    } else if args[0] == "startpos" {
        engine.fen = STARTPOS.to_string();
        engine.turn = Turn::White;
    } else if args[0] == "fen" {
        engine.turn = match args[2] {
            "w" => Turn::White,
            "b" => Turn::Black,
            _ => return true
        };

        let mut iter = args.iter();
        iter.next().unwrap();
        engine.fen = iter.fold(String::new(), |acc, x| acc + x + " ");
    }

    let command_line = args.iter().fold(POSITION_CMD.to_string(), |acc, x| acc + " " + x);
    writeln!(stockfish_stdin, "{}", command_line).unwrap();

    true
}

fn process_go<I, L>(engine: &mut Engine, args: Vec<&str>, mut stockfish_stdin: I, mut stockfish_lines: L) -> bool where
    I: io::Write,
    L: iter::Iterator<Item = String>
{
    let command_line = args.iter().fold(GO_CMD.to_string(), |acc, x| acc + " " + x);
    writeln!(stockfish_stdin, "{}", command_line).unwrap();

    let rng = rand::thread_rng();
    let position = get_position_info_cached(engine.fen.as_str(), engine.master_games, &engine.ratings, &engine.tc, &mut engine.cache).unwrap();
    let best_move = get_position_move(&engine, position.borrow(), rng);

    let mut depth = "1".to_string();
    let mut seldepth = "1".to_string();
    let mut score_unit = "cp".to_string();
    let mut score = "0".to_string();

    loop {
        match stockfish_lines.next() {
            Some(line) => {
                if line.as_str().starts_with(BESTMOVE_RESP) {
                    match best_move {
                        Some(x) => {
                            let games = x.white + x.draws + x.black;
                            println!("{} white {} draws {} black {} games {} lichessdbmove {}", INFO_RESP, x.white*100/games, x.draws*100/games, x.black*100/games, games, x.san);
                            println!("{} depth {} seldepth {} multipv 1 score {} {} pv {}", INFO_RESP, depth, seldepth, score_unit, score, x.uci);
                            println!("{} {}", BESTMOVE_RESP, x.uci);
                        }
                        None => println!("{}", line)
                    }
                    break
                } else {
                    match best_move {
                        Some(_) => {
                            let mut words = line.as_str().split_ascii_whitespace();
                            match words.next() {
                                Some(INFO_RESP) => loop {
                                    match words.next() {
                                        Some(word) => match word {
                                            "depth" => match words.next() {
                                                Some(word) => depth = word.to_string(),
                                                None => break
                                            },
                                            "seldepth" => match words.next() {
                                                Some(word) => seldepth = word.to_string(),
                                                None => break
                                            },
                                            "score" => {
                                                match words.next() {
                                                    Some("cp") => score_unit = "cp".to_string(),
                                                    Some("mate") => score_unit = "mate".to_string(),
                                                    Some(_) => (),
                                                    None => break
                                                }
                                                match words.next() {
                                                    Some(word) => score = word.to_string(),
                                                    None => break
                                                }
                                            },
                                            _ => ()
                                        }
                                        None => break
                                    }
                                }
                                Some(_) => (),
                                None => break
                            }
                        }
                        None => println!("{}", line)
                    }
                }
            }
            None => return false 
        }
    }
    true
}

fn process_stop<I>(mut stockfish_stdin: I) -> bool where
    I: io::Write,
{
    writeln!(stockfish_stdin, "{}", STOP_CMD).unwrap();

    true
}

fn main() -> io::Result<()> 
{
    let mut engine = Engine {
        master_games: false,

        ratings: RatingFilter {
            rating_1600: true,
            rating_1800: true,
            rating_2000: true,
            rating_2200: true,
            rating_2500: true,
        },

        tc: TimeControlFilter {
            bullet: true,
            blitz: true,
            rapid: true,
            classical: true,
        },

        opt_games_min: 30,
        opt_games_pct_min: 1,
        opt_score_pct_min: 0,
        opt_sortby: SortBy::Games,
        opt_variants: 1,
        opt_weightby: WeightBy::Random,

        fen: STARTPOS.to_string(),
        turn: Turn::White,
        cache: HashMap::new(),
    };

    let mut stockfish = process::Command::new("stockfish")
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::inherit())
        .spawn().unwrap();

    let mut stockfish_stdin = stockfish.stdin.as_mut().unwrap();
    let mut stockfish_lines = io::BufReader::new(stockfish.stdout.as_mut().unwrap()).lines().map(|l| l.unwrap());
    match stockfish_lines.next() {
        Some(line) => println!("Lichessdbfish over {}", line),
        None => ()
    }

    let mut lines = io::BufReader::new(io::stdin()).lines().map(|l| l.unwrap());
    while match lines.next() {
        Some(line) => {
            let mut words = line.as_str().split_ascii_whitespace();
            match words.next() {
                Some(word) => {
                    match word {
                        UCI_CMD       => process_uci(&mut stockfish_stdin, &mut stockfish_lines),
                        SETOPTION_CMD => process_setoption(&mut engine, words.collect(), &mut stockfish_stdin),
                        ISREADY_CMD   => process_isready(&mut stockfish_stdin, &mut stockfish_lines),
                        POSITION_CMD  => process_position(&mut engine, words.collect(), &mut stockfish_stdin),
                        GO_CMD        => process_go(&mut engine, words.collect(), &mut stockfish_stdin, &mut stockfish_lines),
                        STOP_CMD      => process_stop(&mut stockfish_stdin),
                        QUIT_CMD      => false,
                        unknown_cmd   => {
                            println!("Unknown command: {}", unknown_cmd);
                            true
                        }
                    }
                }
                None => true
            }
        }
        None => false
    }{}

    writeln!(stockfish_stdin, "{}", QUIT_CMD).unwrap();
    writeln!(stockfish_stdin, "{}", EXIT_CMD).unwrap();
    writeln!(stockfish_stdin, "{}", QUIT_CMD).unwrap();

    stockfish.wait().unwrap();

    Ok(())
}
