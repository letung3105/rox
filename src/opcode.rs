/// Opcode is a byte specifying the action that the VM must take.
///
/// # Notes
///
/// If it was for performances purposes, the following options could be considered:
/// + Having the opcode designed to be as close as possbible to existing lower-level instructions
/// + Having specialized opcode for constant
///
/// We don't have a `Opcode::NotEqual` because we will transform `a != b` to `!(a == b)` to demonstrated
/// that bytecode can deviate from the actual user's code as long as they behave similarly. This is also
/// applied for operator `<=` and operator `>=`.
///
/// `a <= b` does not equals equivalent to `!(a > b)`, similarly with greater and greater or equal.
/// According to [IEEE 754] all comparison operators return `false` when an operand is `NaN`. These
/// are implementation details that we should keep in mind when making a real language.
///
/// [IEEE 754]: https://en.wikipedia.org/wiki/IEEE_754
#[derive(Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    /// Load a constant
    Const = 0,
    /// Load a `nil` value
    Nil = 1,
    /// Load a `true` value
    True = 2,
    /// Load a `false` value
    False = 3,
    /// Pop the top of the stack
    Pop = 4,
    /// Set the value of a global variable
    GetLocal = 5,
    /// Set the value of a local variable
    SetLocal = 6,
    /// Get the value of a global variable
    GetGlobal = 7,
    /// Set the value of a global variable
    SetGlobal = 8,
    /// Pop the top of the stack and define a variable initialized with that value.
    DefineGlobal = 9,
    /// Get a variable through its upvalue
    GetUpvalue = 10,
    /// Set a variable through its upvalue
    SetUpvalue = 11,
    /// Get the value of a property on the class instance
    GetProperty = 12,
    /// Set the value of a property on the class instance
    SetProperty = 13,
    /// Get the super class instance of the current class
    GetSuper = 14,
    /// Check for inequality between 2 operands.
    NE = 15,
    /// Check for equality between 2 operands.
    EQ = 16,
    /// Compare if the first operand is greater than the second
    GT = 17,
    /// Compare if the first operand is greater than or equal the second
    GE = 18,
    /// Compare if the first operand is less than the second
    LT = 19,
    /// Compare if the first operand is less than or equal the second
    LE = 20,
    /// Add two number operands or two string operands
    Add = 21,
    /// Subtract two number operands
    Sub = 22,
    /// Multiply two number operands
    Mul = 23,
    /// Divide two number operands
    Div = 24,
    /// Apply logical `not` to a single boolean operand
    Not = 25,
    /// Negate a single number operand
    Neg = 26,
    /// Print an expression in human readable format
    Print = 27,
    /// Jump forward for n instructions
    Jump = 28,
    /// Jump forward for n instructions if current stack top is truthy
    JumpIfTrue = 29,
    /// Jump forward for n instructions if current stack top is falsey
    JumpIfFalse = 30,
    /// Jump backward for n instructions
    Loop = 31,
    /// Make a function call
    Call = 32,
    /// Invoke method call directly without going though an access operation
    Invoke = 33,
    /// Invoke super call directly without going though an access operation
    SuperInvoke = 34,
    /// Add a new closure
    Closure = 35,
    /// Move captured value to the heap
    CloseUpvalue = 36,
    /// Return from the current function
    Ret = 37,
    /// Create a class and bind it to a name
    Class = 38,
    /// Create a inheritance relation between two classes
    Inherit = 39,
    /// Define a method
    Method = 40,
}

impl From<Opcode> for u8 {
    fn from(op: Opcode) -> Self {
        op as u8
    }
}

impl From<u8> for Opcode {
    fn from(byte: u8) -> Self {
        match byte {
            0 => Opcode::Const,
            1 => Opcode::Nil,
            2 => Opcode::True,
            3 => Opcode::False,
            4 => Opcode::Pop,
            5 => Opcode::GetLocal,
            6 => Opcode::SetLocal,
            7 => Opcode::GetGlobal,
            8 => Opcode::SetGlobal,
            9 => Opcode::DefineGlobal,
            10 => Opcode::GetUpvalue,
            11 => Opcode::SetUpvalue,
            12 => Opcode::GetProperty,
            13 => Opcode::SetProperty,
            14 => Opcode::GetSuper,
            15 => Opcode::NE,
            16 => Opcode::EQ,
            17 => Opcode::GT,
            18 => Opcode::GE,
            19 => Opcode::LT,
            20 => Opcode::LE,
            21 => Opcode::Add,
            22 => Opcode::Sub,
            23 => Opcode::Mul,
            24 => Opcode::Div,
            25 => Opcode::Not,
            26 => Opcode::Neg,
            27 => Opcode::Print,
            28 => Opcode::Jump,
            29 => Opcode::JumpIfTrue,
            30 => Opcode::JumpIfFalse,
            31 => Opcode::Loop,
            32 => Opcode::Call,
            33 => Opcode::Invoke,
            34 => Opcode::SuperInvoke,
            35 => Opcode::Closure,
            36 => Opcode::CloseUpvalue,
            37 => Opcode::Ret,
            38 => Opcode::Class,
            39 => Opcode::Inherit,
            40 => Opcode::Method,
            b => panic!("Unknown byte-code '{b}'"),
        }
    }
}
