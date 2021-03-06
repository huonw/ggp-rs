use regex::Regex;

use time::PreciseTime;

use std::collections::{HashMap, BTreeSet};
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::cell::RefCell;

use Player;
use util::{cross_product, create_does};
use prover::{Prover, query_builder};
use gdl::{self, constants, Description, Sentence, Role, Move, Score, Function, Constant};
use gdl::Clause::{SentenceClause, RuleClause};
use gdl::Sentence::{RelSentence, PropSentence};
use gdl::Term::ConstTerm;

use self::MatchState::{Started, Playing, Finished};
use self::Request::{StartRequest, PlayRequest, StopRequest, InfoRequest, AbortRequest,
                    UnknownRequest};

#[derive(Eq, PartialEq)]
enum MatchState {
    Started, Playing, Finished
}

/// A description of a game
pub struct Game {
    match_state: MatchState,
    roles: Vec<Role>,
    role: Role,
    start_clock: u32,
    play_clock: u32,
    move_start_time: PreciseTime,
    cur_state: State,
    init_state: State,
    prover: Prover,
    cache: RefCell<Cache>,
}

struct Cache {
    cache: HashMap<State, CacheEntry>
}

impl Cache {
    fn new() -> Cache {
        Cache { cache: HashMap::new() }
    }

    fn get(&mut self, state: &State) -> &mut CacheEntry {
        // We don't use cache.entry() as the cost of cloning `state` will probably not be worth it
        if self.cache.contains_key(state) {
            self.cache.get_mut(state).unwrap()
        } else {
            self.cache.insert(state.clone(), CacheEntry::new());
            self.cache.get_mut(state).unwrap()
        }
    }
}

struct CacheEntry {
    legals: HashMap<Role, Vec<Move>>,
    next: HashMap<Vec<Move>, State>,
    terminal: Option<bool>,
    goals: HashMap<Role, Score>
}

impl CacheEntry {
    fn new() -> CacheEntry {
        CacheEntry { legals: HashMap::new(), next: HashMap::new(), terminal: None,
                     goals: HashMap::new() }
    }
}

/// The state of a game, containing all `Sentence`s that are true in this state
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct State {
    pub props: BTreeSet<Sentence>
}

impl State {
    pub fn new() -> State {
        State { props: BTreeSet::new() }
    }
}

impl Game {
    fn new(role: Role, start_clock: u32, play_clock: u32, desc: Description) -> Game {
        let roles = Game::compute_roles(&desc);
        let prover = Prover::new(desc);
        let init_state = prover.ask(query_builder::init_query(), State::new()).into_state();

        Game { match_state: Started, roles: roles, role: role, start_clock: start_clock,
               play_clock: play_clock, init_state: init_state.clone(), cur_state: init_state,
               prover: prover, cache: RefCell::new(Cache::new()),
               move_start_time: PreciseTime::now() }
    }

    fn compute_roles(desc: &Description) -> Vec<Role> {
        let mut roles = Vec::new();
        for clause in desc.clauses.iter() {
            if let &SentenceClause(ref s) = clause {
                if let &RelSentence(ref r) = s {
                    if r.name.name == "role" {
                        assert_eq!(r.args.len(), 1);
                        match &r.args[0] {
                            &ConstTerm(ref c) => roles.push(Role::new(c.name.to_string())),
                            _ => panic!("Expected constant term")
                        }
                    }
                }
            }
        }
        roles
    }

    /// Returns true if `state` is a terminal state
    pub fn is_terminal(&self, state: &State) -> bool {
        let mut cache = self.cache.borrow_mut();
        let entry = cache.get(state);
        match entry.terminal {
            Some(b) => b,
            None => {
                let res = self.prover.prove(query_builder::terminal_query(), state.clone());
                entry.terminal = Some(res);
                res
            }
        }
    }

    /// Returns the roles of the game
    pub fn get_roles(&self) -> &Vec<Role> {
        &self.roles
    }

    /// Returns role of the player
    pub fn get_role(&self) -> &Role {
        &self.role
    }

    /// Returns the initial state of the game
    pub fn get_initial_state(&self) -> &State {
        &self.init_state
    }

    /// Returns the current state of the game
    pub fn get_current_state(&self) -> &State {
        &self.cur_state
    }

    /// Returns all legal moves for role `role` in the state `state`
    pub fn get_legal_moves(&self, state: &State, role: &Role) -> Vec<Move> {
        let mut cache = self.cache.borrow_mut();
        let mut entry = cache.get(state);
        match entry.legals.entry(role.clone()) {
            Occupied(e) => e.get().clone(),
            Vacant(e) => {
                let res = self.prover.ask(query_builder::legal_query(role),
                                          state.clone()).into_moves();
                e.insert(res.clone());
                res
            }
        }
    }

    /// Returns all legal joint moves in state `state`. Each element of the resulting vector is a
    /// vector that contains a legal move for each player in the game.
    pub fn get_legal_joint_moves(&self, state: &State) -> Vec<Vec<Move>> {
        let mut legal_moves = Vec::new();
        for role in self.roles.iter() {
            legal_moves.push(self.get_legal_moves(state, role));
        }

        cross_product(legal_moves)
    }

    /// Returns the score for each player in state `state`
    pub fn get_goals(&self, state: &State) -> Vec<Score> {
        let mut res = Vec::new();
        for role in self.roles.iter() {
            res.push(self.get_goal(state, role))
        }
        res
    }

    /// Returns the score for role `role` in state `state`
    pub fn get_goal(&self, state: &State, role: &Role) -> Score {
        let mut cache = self.cache.borrow_mut();
        let mut entry = cache.get(state);
        match entry.goals.entry(role.clone()) {
            Occupied(e) => e.get().clone(),
            Vacant(e) => {
                let res = self.prover.ask(query_builder::goal_query(role),
                                          state.clone()).into_score();
                e.insert(res.clone());
                res
            }
        }
    }

    /// Gets all possible next states given state `state`
    pub fn get_next_states(&self, state: &State) -> Vec<State> {
        let mut res = Vec::new();
        for moves in self.get_legal_joint_moves(state) {
            res.push(self.get_next_state(state, &moves));
        }
        res
    }

    /// Gets the next state given state `state` and each players' next move
    pub fn get_next_state(&self, state: &State, moves: &[Move]) -> State {
        if moves[0] == *constants::NIL_MOVE {
            return state.clone();
        }
        assert_eq!(moves.len(), self.roles.len());

        let mut cache = self.cache.borrow_mut();
        let mut entry = cache.get(state);
        match entry.next.entry(moves.iter().cloned().collect()) {
            Occupied(e) => e.get().clone(),
            Vacant(e) => {
                let mut s = state.clone();
                for (m, r) in moves.iter().zip(self.roles.iter()) {
                    s.props.insert(create_does(r, m));
                }

                let res = self.prover.ask(query_builder::next_query(), s).into_state();
                e.insert(res.clone());
                res
            }
        }
    }

    /// Gets the start clock time
    pub fn get_start_clock(&self) -> u32 {
        self.start_clock
    }

    /// Gets the play clock time
    pub fn get_play_clock(&self) -> u32 {
        self.play_clock
    }

    pub fn move_start_time(&self) -> PreciseTime {
        self.move_start_time
    }

    fn update(&mut self, moves: &[Move]) {
        if self.match_state != Playing {
            self.match_state = Playing;
        }
        let new_state = self.get_next_state(&self.cur_state, moves);
        if cfg!(debug_assertions) {
            let old_props: Vec<_> =
                self.cur_state.props.difference(&new_state.props).cloned().collect();
            let new_props: Vec<_> =
                new_state.props.difference(&self.cur_state.props).cloned().collect();
            debug!("Removed propositions: {:?}", old_props);
            debug!("Added propositions: {:?}", new_props);
        }
        self.cur_state = new_state;

        // TODO: How often to clear cache?
        self.cache.borrow_mut().cache.clear();
    }

    fn finish(&mut self, moves: &[Move]) {
        self.cur_state = self.get_next_state(&self.cur_state, moves);
        self.match_state = Finished;
    }
}

pub struct GameManager<P> {
    player: P,
    games: HashMap<String, Game>
}

#[derive(Debug, Eq, PartialEq)]
enum Request {
    StartRequest(String, Role, Description, u32, u32),
    PlayRequest(String, Vec<Move>),
    StopRequest(String, Vec<Move>),
    InfoRequest,
    AbortRequest(String),
    UnknownRequest(String)
}

impl<P: Player> GameManager<P> {
    pub fn new(p: P) -> GameManager<P> {
        GameManager { games: HashMap::new(), player: p }
    }

    pub fn handle(&mut self, request: String) -> String {
        let mut parser = RequestParser::new(request);
        let req = parser.parse();
        debug!("Parsed request: {:?}", req);
        match req {
            StartRequest(match_id, role, desc, start_clock, play_clock) =>
                self.handle_start(match_id, role, desc, start_clock, play_clock),
            PlayRequest(match_id, moves) => self.handle_play(match_id, moves),
            StopRequest(match_id, moves) => self.handle_stop(match_id, moves),
            InfoRequest => "available".to_string(),
            AbortRequest(match_id) =>
                self.handle_stop(match_id, vec![constants::NIL_MOVE.clone()]),
            UnknownRequest(req) => panic!("Unknown request: {}", req)
        }
    }

    fn handle_start(&mut self, match_id: String, role: Role, desc: Description,
                    start_clock: u32, play_clock: u32) -> String {
        info!("Handling start request" );
        let game = Game::new(role, start_clock, play_clock, desc);
        self.player.meta_game(&game);
        self.games.insert(match_id, game);
        info!("Sending 'ready'");
        "ready".to_string()
    }

    fn handle_play(&mut self, match_id: String, moves: Vec<Move>) -> String {
        info!("Handling play request");
        let game = self.games.get_mut(&match_id).expect("Match doesn't exist");
        game.update(&moves);
        game.move_start_time = PreciseTime::now();
        let m = self.player.select_move(game);
        let mov = m.to_string();
        info!("Sending {}", mov);
        mov
    }

    fn handle_stop(&mut self, match_id: String, moves: Vec<Move>) -> String {
        info!("Handling stop request");
        let game = self.games.get_mut(&match_id).expect("Match doesn't exist");
        game.finish(&moves);
        self.player.stop(game);
        info!("Sending 'done'");
        "done".to_string()
    }
}

struct RequestParser {
    s: String,
    pos: usize
}

impl RequestParser {
    fn new(req: String) -> RequestParser {
        RequestParser { s: req, pos: 0 }
    }

    fn peek(&self) -> char {
        assert!(self.s.len() > self.pos);
        self.s[self.pos..].chars().next().unwrap()
    }

    fn consume(&mut self, c: char) -> Result<(), String> {
        let p = self.peek();
        if p == c {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("Expected {} at position {} but got {}", c, self.pos, p))
        }
    }

    fn consume_str(&mut self, s: &str) -> Result<(), String> {
        for c in s.chars() {
            try!(self.consume(c))
        }
        Ok(())
    }

    fn consume_whitespace(&mut self) {
        while self.peek() == ' ' {
            self.consume(' ').unwrap();
        }
    }

    fn next(&mut self) -> char {
        let c = self.peek();
        self.consume(c).unwrap();
        c
    }

    fn parse(&mut self) -> Request {
        self.consume('(').unwrap();
        self.consume_whitespace();
        match self.peek() {
            'a' => self.parse_abort(),
            'i' => InfoRequest,
            'p' => self.parse_play(),
            's' => {
                self.consume('s').unwrap();
                self.consume('t').unwrap();
                match self.peek() {
                    'a' => self.parse_start(),
                    'o' => self.parse_stop(),
                    _ => UnknownRequest(self.s.clone())
                }
            },
            _ => UnknownRequest(self.s.clone())
        }
    }

    fn parse_abort(&mut self) -> Request {
        self.consume_str("abort").unwrap();
        self.consume_whitespace();

        let match_id = self.parse_string();
        self.consume_whitespace();

        self.consume(')').unwrap();

        AbortRequest(match_id)
    }

    fn parse_start(&mut self) -> Request {
        self.consume_str("art").unwrap();
        self.consume_whitespace();

        let match_id = self.parse_string();
        self.consume_whitespace();

        let role = Role::new(self.parse_string());
        self.consume_whitespace();

        let desc = self.parse_gdl();
        self.consume_whitespace();

        let start_clock = self.parse_int();
        self.consume_whitespace();

        let play_clock = self.parse_int();
        self.consume_whitespace();

        self.consume(')').unwrap();
        StartRequest(match_id, role, desc, start_clock, play_clock)
    }

    fn parse_play(&mut self) -> Request {
        self.consume_str("play").unwrap();
        self.consume_whitespace();

        let match_id = self.parse_string();
        self.consume_whitespace();

        let moves = self.parse_move_list();
        self.consume_whitespace();

        self.consume(')').unwrap();
        PlayRequest(match_id, moves)
    }

    fn parse_stop(&mut self) -> Request {
        self.consume_str("op").unwrap();
        self.consume_whitespace();

        let match_id = self.parse_string();
        self.consume_whitespace();

        let moves = self.parse_move_list();
        self.consume_whitespace();

        self.consume(')').unwrap();
        StopRequest(match_id, moves)
    }

    fn parse_string(&mut self) -> String {
        let mut res = String::new();
        let mut c = self.peek();
        while (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
            || c == '_' || c == '.' {
            res.push(self.next());
            c = self.peek();
        }
        res
    }

    fn parse_move_list(&mut self) -> Vec<Move> {
        match self.peek() {
            '(' => (),
            _ => return vec![Move::new(Constant::new(self.parse_string()).into())]
        }
        self.consume('(').unwrap();
        self.consume_whitespace();
        let r = match Regex::new(r"(?P<relation>\([^)]+\))|(?P<prop>[a-zA-Z0-9_]+)") {
            Ok(r) => r,
            Err(e) => panic!("{}", e)
        };
        let mut moves = Vec::new();
        let remaining_str = &self.s[self.pos..].to_string();
        for cap in r.captures_iter(&remaining_str) {
            match cap.name("relation") {
                Some(r) => {
                    self.consume_str(&r).unwrap();
                    self.consume_whitespace();
                    let mut desc = gdl::parse(r);
                    assert_eq!(desc.clauses.len(), 1);
                    let clause = desc.clauses.swap_remove(0);
                    let sentence = match clause {
                        SentenceClause(s) => s,
                        RuleClause(_) => panic!("Expected SentenceClause")
                    };
                    match sentence {
                        RelSentence(r) => moves.push(Move::new(Function::new(r.name,
                                                                             r.args).into())),
                        PropSentence(_) => panic!("Expected RelSentence")
                    }
                },
                None => {
                    let s = cap.name("prop").expect("Move must be a proposition");
                    self.consume_str(&s).unwrap();
                    self.consume_whitespace();
                    moves.push(Move::new(Constant::new(s).into()));
                }
            }
        }

        self.consume(')').unwrap();
        moves
    }

    fn parse_gdl(&mut self) -> Description {
        let r = match Regex::new(r"\((.*)\).") {
            Ok(r) => r,
            Err(e) => panic!("{}", e)
        };
        let gdl = {
            let caps = r.captures(&self.s[self.pos..]).unwrap();
            assert_eq!(caps.len(), 2);
            caps.at(1).unwrap().to_string()
        };
        self.consume('(').unwrap();
        self.consume_str(&gdl).unwrap();
        self.consume(')').unwrap();
        gdl::parse(&gdl)
    }

    fn parse_int(&mut self) -> u32 {
        let mut res = String::new();
        let mut c = self.peek();
        while c >= '0' && c <= '9' {
            res.push(self.next());
            c = self.peek();
        }
        res.parse().unwrap()
    }
}

#[cfg(test)]
mod test {
    extern crate env_logger;
    use Player;
    use super::{GameManager, Game, RequestParser};
    use super::Request::PlayRequest;
    use gdl::{Move, Constant, Function};

    struct LegalPlayer;
    impl Player for LegalPlayer {
        fn get_name(&self) -> String {
            "LegalPlayer".to_string()
        }

        fn select_move(&mut self, game: &Game) -> Move {
            let state = game.get_current_state();
            let role = game.get_role();
            let mut moves = game.get_legal_moves(state, role);
            assert!(!moves.is_empty());
            moves.swap_remove(0)
        }
    }

    #[test]
    fn test_play_nil() {
        let mut gm = GameManager::new(LegalPlayer);
        assert_eq!(
            &gm.handle("(start match_id black ((role black) (input noop) \
                        (legal black noop)) 10 5)".to_string()),
            "ready");
        assert_eq!(&gm.handle("(play match_id nil)".to_string()), "noop");
    }

    #[test]
    fn test_play_turns() {
        env_logger::init().unwrap();
        let mut gm = GameManager::new(LegalPlayer);
        assert_eq!(
            &gm.handle("(start match_id black ((role black) (role red) \
                        (<= (legal black noop) (true (control red))) \
                        (<= (legal red noop) (true (control black))) \
                        (<= (legal black p) (true (control black))) \
                        (<= (legal red p) (true (control red))) \
                        (init (control black)) \
                        (<= (next (control black)) (true (control red))) \
                        (<= (next (control red)) (true (control black)))) 10 5)".to_string()),
            "ready");
        assert_eq!(&gm.handle("(play match_id nil)".to_string()), "p");
        assert_eq!(&gm.handle("(play match_id (p noop))".to_string()), "noop");
        assert_eq!(&gm.handle("(play match_id (noop p))".to_string()), "p");
    }

    #[test]
    fn test_parse_play() {
        let mut parser = RequestParser::new("(play testmatch_1 ((move 4 1 3 1) noop))".to_string());
        let expected = PlayRequest("testmatch_1".to_string(),
                                   vec![Move::new(
                                       Function::new("move",
                                                     vec![Constant::new("4").into(),
                                                          Constant::new("1").into(),
                                                          Constant::new("3").into(),
                                                          Constant::new("1").into()]).into()),
                                        Move::new(Constant::new("noop").into())]);
        assert_eq!(parser.parse(), expected);
    }
}
