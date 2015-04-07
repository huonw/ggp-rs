use game_manager::State;

pub use gdl_parser::{Description, Clause, Rule, Sentence, Term, Literal, Constant, Variable,
                     Function, Relation, Proposition, Not, Or, Distinct};
pub use gdl_parser::parse;

pub type Score = u8;

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Role {
    name: String
}

impl Role {
    pub fn new(name: String) -> Role {
        Role { name: name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

pub struct Move;

impl ToString for Move {
    fn to_string(&self) -> String {
        "".to_string() // TODO
    }
}
