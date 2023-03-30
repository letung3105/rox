//! Implementation of the bytecode virtual machine.

use std::{
    collections::{HashMap, HashSet},
    ops::{Add, Div, Mul, Neg, Not, Sub},
    ptr::NonNull,
    rc::Rc,
};

use crate::{
    compile::Parser,
    heap::Heap,
    object::{
        NativeFun, ObjClosure, ObjFun, ObjUpvalue, Object, ObjectContent, ObjectError, ObjectRef,
    },
    opcode::Opcode,
    value::{Value, ValueError},
    InterpretError,
};

#[cfg(feature = "dbg-execution")]
use crate::chunk::disassemble_instruction;

/// The max number of values can be put onto the virtual machine's stack.
const VM_STACK_SIZE: usize = 256;

/// The max number of call frames can be handled by the virtual machine.
const VM_FRAMES_MAX: usize = 64;

/// An enumeration of potential errors occur when running the bytecodes.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum RuntimeError {
    /// Can't parse a byte as an opcode.
    #[error(transparent)]
    InvalidOpcode(#[from] num_enum::TryFromPrimitiveError<Opcode>),

    /// Can't perform some operations given the current value(s).
    #[error(transparent)]
    Value(#[from] ValueError),

    /// Can't perform some operations given the current object(s).
    #[error(transparent)]
    Object(#[from] ObjectError),

    /// Overflown the virtual machine's stack.
    #[error("Stack overflow.")]
    StackOverflow,

    /// Can't find a variable in scope.
    #[error("Undefined variable '{0}'.")]
    UndefinedVariable(String),

    /// Called a function/method with incorrect number of arguments.
    #[error("Expected {arity} arguments but got {argc}.")]
    BadArgumentsCount {
        /// The arity of the function.
        arity: u8,
        /// The number of arguments given.
        argc: u8,
    },
}

/// A bytecode virtual machine for the Lox programming language.
pub struct VirtualMachine {
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    open_upvalues: Vec<ObjectRef>,
    globals: HashMap<Rc<str>, Value>,
    grey_objects: Vec<ObjectRef>,
    black_objects: HashSet<NonNull<Object>>,
    heap: Heap,
}

impl Default for VirtualMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualMachine {
    /// Create a new virtual machine that prints to the given output.
    pub fn new() -> Self {
        let mut vm = Self {
            stack: Vec::default(),
            frames: Vec::default(),
            open_upvalues: Vec::default(),
            globals: HashMap::default(),
            grey_objects: Vec::default(),
            black_objects: HashSet::default(),
            heap: Heap::default(),
        };
        vm.define_native("clock", 0, clock_native)
            .expect("Native function must be defined.");
        vm
    }
}

impl VirtualMachine {
    /// Compile and execute the given source code.
    pub fn interpret(&mut self, src: &str) -> Result<(), InterpretError> {
        let parser = Parser::new(src, &mut self.heap);
        let fun = parser.compile().ok_or(InterpretError::Compile)?;
        self.run(fun).map_err(|err| {
            eprintln!("{err}");
            if let Err(err) = self.trace_calls() {
                eprintln!("{err}");
            };
            self.stack.clear();
            InterpretError::Runtime
        })
    }

    fn run(&mut self, fun: ObjFun) -> Result<(), RuntimeError> {
        // Push the constant onto the stack so GC won't remove it while allocating the function.
        for constant in &fun.chunk.constants {
            self.stack_push(*constant)?;
        }
        let constant_count = fun.chunk.constants.len();
        let fun_object = self.alloc_fun(fun);
        // Remove all added constants.
        self.stack_remove_top(constant_count);

        // Push the function onto the stack so GC won't remove it while we allocating the closure.
        self.stack_push(Value::Object(fun_object))?;
        // Create a closure for the script function. Note that script can't have upvalues.
        let closure_object = self.alloc_closure(ObjClosure::new(fun_object, Vec::new()));
        // Pop the fun object as we no longer need it.
        self.stack_pop();

        // Push the closure onto the stack so GC won't remove for the entire runtime.
        self.stack_push(Value::Object(closure_object))?;
        // Start running the closure.
        let mut task = Task::new(self);
        task.call_closure(closure_object, 0)
            .and_then(|_| task.run())
    }

    fn frame(&self) -> &CallFrame {
        let index = self.frames.len() - 1;
        &self.frames[index]
    }

    fn frame_mut(&mut self) -> &mut CallFrame {
        let index = self.frames.len() - 1;
        &mut self.frames[index]
    }

    fn frames_push(&mut self, frame: CallFrame) -> Result<usize, RuntimeError> {
        let frame_count = self.frames.len();
        if frame_count == VM_FRAMES_MAX {
            return Err(RuntimeError::StackOverflow);
        }
        self.frames.push(frame);
        Ok(frame_count)
    }

    fn frames_pop(&mut self) -> CallFrame {
        self.frames.pop().expect("Stack empty.")
    }

    fn stack_push(&mut self, value: Value) -> Result<usize, RuntimeError> {
        let stack_size = self.stack.len();
        if stack_size == VM_STACK_SIZE {
            return Err(RuntimeError::StackOverflow);
        }
        self.stack.push(value);
        Ok(stack_size)
    }

    fn stack_pop(&mut self) -> Value {
        self.stack.pop().expect("Stack empty.")
    }

    fn stack_top(&self, n: usize) -> &Value {
        let index = self.stack.len() - n - 1;
        &self.stack[index]
    }

    fn stack_top_mut(&mut self, n: usize) -> &mut Value {
        let index = self.stack.len() - n - 1;
        &mut self.stack[index]
    }

    fn stack_remove_top(&mut self, n: usize) {
        self.stack.drain(self.stack.len() - n..);
    }

    fn trace_calls(&self) -> Result<(), RuntimeError> {
        for frame in self.frames.iter().rev() {
            let closure_ref = frame.closure();
            let fun_ref = closure_ref.fun();
            let line = fun_ref.chunk.get_line(frame.ip - 1);
            match &fun_ref.name {
                None => eprintln!("{line} in script."),
                Some(s) => eprintln!("{line} in {s}()."),
            }
        }
        Ok(())
    }

    fn define_native(
        &mut self,
        name: &str,
        arity: u8,
        call: fn(&[Value]) -> Value,
    ) -> Result<(), RuntimeError> {
        let fun_name = self.heap.intern(String::from(name));
        let fun = NativeFun { arity, call };
        let fun_object = self.alloc_native_fun(fun);
        self.stack_push(Value::Object(fun_object))?;
        self.globals.insert(fun_name, *self.stack_top(0));
        self.stack_pop();
        Ok(())
    }

    fn alloc_string(&mut self, s: String) -> ObjectRef {
        self.gc();
        self.heap.alloc_string(s)
    }

    fn alloc_upvalue(&mut self, upvalue: ObjUpvalue) -> ObjectRef {
        self.gc();
        self.heap.alloc_upvalue(upvalue)
    }

    fn alloc_closure(&mut self, closure: ObjClosure) -> ObjectRef {
        self.gc();
        self.heap.alloc_closure(closure)
    }

    fn alloc_fun(&mut self, fun: ObjFun) -> ObjectRef {
        self.gc();
        self.heap.alloc_fun(fun)
    }

    fn alloc_native_fun(&mut self, native_fun: NativeFun) -> ObjectRef {
        self.gc();
        self.heap.alloc_native_fun(native_fun)
    }

    fn gc(&mut self) {
        if self.heap.size() <= self.heap.next_gc() {
            return;
        }

        #[cfg(feature = "dbg-heap")]
        let before = {
            println!("-- gc begin");
            self.heap.size()
        };

        self.mark_sweep();

        #[cfg(feature = "dbg-heap")]
        {
            let after = self.heap.size();
            let next = self.heap.next_gc();
            let delta = before.abs_diff(after);
            println!("-- gc end");
            println!("   collected {delta} bytes (from {before} to {after}) next at {next}");
        };
    }

    fn mark_sweep(&mut self) {
        self.grey_objects.clear();
        self.black_objects.clear();
        self.mark_roots();
        while let Some(mut grey_object) = self.grey_objects.pop() {
            self.black_objects.insert(grey_object.0);
            grey_object.mark_references(&mut self.grey_objects, &self.black_objects)
        }
        self.heap.sweep(&self.black_objects);
    }

    fn mark_roots(&mut self) {
        for value in &mut self.stack {
            if let Value::Object(o) = value {
                o.mark(&mut self.grey_objects, &self.black_objects);
            }
        }
        for frame in &mut self.frames {
            frame
                .closure
                .mark(&mut self.grey_objects, &self.black_objects);
        }
        for upvalue in &mut self.open_upvalues {
            upvalue.mark(&mut self.grey_objects, &self.black_objects);
        }
        for value in self.globals.values_mut() {
            if let Value::Object(ref mut o) = value {
                o.mark(&mut self.grey_objects, &self.black_objects);
            }
        }
    }

    #[cfg(feature = "dbg-execution")]
    fn trace_stack(&self) {
        print!("          ");
        for value in self.stack.iter() {
            print!("[ {value} ]");
        }
        println!();
    }
}

fn clock_native(_args: &[Value]) -> Value {
    let start = std::time::SystemTime::now();
    let since_epoch = start
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards");
    Value::Number(since_epoch.as_secs_f64())
}

/// A task is the structure responsible for executing a single chunk.
struct Task<'vm> {
    vm: &'vm mut VirtualMachine,
}

impl<'vm> Task<'vm> {
    /// Create a new task given the chunk to be run.
    fn new(vm: &'vm mut VirtualMachine) -> Self {
        Self { vm }
    }

    /// Read the next byte in the stream of bytecode instructions.
    fn read_byte(&mut self) -> Result<u8, RuntimeError> {
        self.vm.frame_mut().read_byte()
    }

    /// Read the next 2 bytes in the stream of bytecode instructions.
    fn read_short(&mut self) -> Result<u16, RuntimeError> {
        self.vm.frame_mut().read_short()
    }

    /// Read the next byte in the stream of bytecode instructions and return the constant at the
    /// index given by the byte.
    fn read_constant(&mut self) -> Result<Value, RuntimeError> {
        self.vm.frame_mut().read_constant()
    }
}

impl<'vm> Task<'vm> {
    fn run(&mut self) -> Result<(), RuntimeError> {
        loop {
            #[cfg(feature = "dbg-execution")]
            {
                self.vm.trace_stack();
                let frame = self.vm.frame();
                let closure_ref = frame.closure();
                let fun_ref = closure_ref.fun();
                disassemble_instruction(&fun_ref.chunk, frame.ip);
            }

            match Opcode::try_from(self.read_byte()?)? {
                Opcode::Const => self.constant()?,
                Opcode::Nil => {
                    self.vm.stack_push(Value::Nil)?;
                }
                Opcode::True => {
                    self.vm.stack_push(Value::Bool(true))?;
                }
                Opcode::False => {
                    self.vm.stack_push(Value::Bool(false))?;
                }
                Opcode::Pop => {
                    self.vm.stack_pop();
                }
                Opcode::GetLocal => self.get_local()?,
                Opcode::SetLocal => self.set_local()?,
                Opcode::GetGlobal => self.get_global()?,
                Opcode::SetGlobal => self.set_global()?,
                Opcode::DefineGlobal => self.defined_global()?,
                Opcode::GetUpvalue => self.get_upvalue()?,
                Opcode::SetUpvalue => self.set_upvalue()?,
                Opcode::NE => self.ne()?,
                Opcode::EQ => self.eq()?,
                Opcode::GT => self.gt()?,
                Opcode::GE => self.ge()?,
                Opcode::LT => self.lt()?,
                Opcode::LE => self.le()?,
                Opcode::Add => self.add()?,
                Opcode::Sub => self.sub()?,
                Opcode::Mul => self.mul()?,
                Opcode::Div => self.div()?,
                Opcode::Not => self.not()?,
                Opcode::Neg => self.neg()?,
                Opcode::Print => self.print()?,
                Opcode::Jump => self.jump(JumpDirection::Forward)?,
                Opcode::JumpIfTrue => self.jump_if_true()?,
                Opcode::JumpIfFalse => self.jump_if_false()?,
                Opcode::Loop => self.jump(JumpDirection::Backward)?,
                Opcode::Call => self.call()?,
                Opcode::Closure => self.closure()?,
                Opcode::CloseUpvalue => {
                    self.close_upvalues(self.vm.stack.len() - 1)?;
                    self.vm.stack_pop();
                }
                Opcode::Ret => {
                    if self.ret()? {
                        break;
                    }
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    /// Get the value of the variable capture by an upvalue.
    fn get_upvalue(&mut self) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte()?;
        let frame = self.vm.frame();
        let closure = frame.closure();
        let upvalue_object = closure.upvalues[upvalue_slot as usize];
        let upvalue = upvalue_object.as_upvalue()?;
        match *upvalue {
            // Value is on the stack.
            ObjUpvalue::Open(stack_slot) => {
                let value = self.vm.stack[stack_slot];
                self.vm.stack_push(value)?;
            }
            // Value is on the heap.
            ObjUpvalue::Closed(value) => {
                self.vm.stack_push(value)?;
            }
        }
        Ok(())
    }

    /// Set the value of the variable capture by an upvalue.
    fn set_upvalue(&mut self) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte()?;
        let value = *self.vm.stack_top(0);
        let closure = self.vm.frame_mut().closure_mut();
        let upvalue_object = &mut closure.upvalues[upvalue_slot as usize];
        let upvalue = upvalue_object.as_upvalue_mut()?;
        match upvalue {
            // Value is on the stack.
            ObjUpvalue::Open(stack_slot) => {
                let stack_slot = *stack_slot;
                self.vm.stack[stack_slot] = value;
            }
            // Value is on the heap.
            ObjUpvalue::Closed(v) => *v = value,
        }
        Ok(())
    }

    fn closure(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        let fun_object = constant.as_object()?;
        let fun = fun_object.as_fun()?;

        let upvalue_count = fun.upvalue_count as usize;
        let mut upvalues = Vec::with_capacity(upvalue_count);
        for _ in 0..upvalue_count {
            let is_local = self.read_byte()? == 1;
            let index = self.read_byte()? as usize;
            if is_local {
                upvalues.push(self.capture_upvalue(self.vm.frame().slot + index)?);
            } else {
                let closure = self.vm.frame().closure();
                upvalues.push(closure.upvalues[index]);
            }
        }

        let closure = self.vm.alloc_closure(ObjClosure::new(fun_object, upvalues));
        self.vm.stack_push(Value::Object(closure))?;

        Ok(())
    }

    /// Create an open upvalue capturing the variable at the stack index given by `location`.
    fn capture_upvalue(&mut self, location: usize) -> Result<ObjectRef, RuntimeError> {
        // Check if there's an existing open upvalues that references the same stack slot. We want
        // to close over a variable not a value. Thus, closures can shared data through the same
        // captured variable.
        for obj in self.vm.open_upvalues.iter() {
            let upvalue = obj.as_upvalue()?;
            if let ObjUpvalue::Open(loc) = *upvalue {
                if loc == location {
                    return Ok(*obj);
                }
            }
        }
        // Make a new open upvalue.
        let upvalue = self.vm.alloc_upvalue(ObjUpvalue::Open(location));
        self.vm.open_upvalues.push(upvalue);
        Ok(upvalue)
    }

    // Close all upvalues whose referenced stack slot went out of scope. Here, `last` is the lowest
    // stack slot that went out of scope.
    fn close_upvalues(&mut self, last: usize) -> Result<(), RuntimeError> {
        for obj in self.vm.open_upvalues.iter_mut() {
            let upvalue = obj.as_upvalue_mut()?;
            // Check if we reference a slot that went out of scope.
            let stack_slot = match upvalue {
                ObjUpvalue::Open(slot) if *slot >= last => Some(slot),
                _ => None,
            };
            // Hoist the variable up into the upvalue so it can live after the stack frame is pop.
            if let Some(slot) = stack_slot {
                *upvalue = ObjUpvalue::Closed(self.vm.stack[*slot]);
            }
        }
        // remove closed upvalues from list of open upvalues
        self.vm
            .open_upvalues
            .retain(|v| matches!(&v.content, ObjectContent::Upvalue(ObjUpvalue::Open(_))));
        Ok(())
    }

    /// Return from a function call
    fn ret(&mut self) -> Result<bool, RuntimeError> {
        // Get the result of the function.
        let result = self.vm.stack_pop();
        // The compiler didn't emit Opcode::CloseUpvalue at the end of each of the outer most scope
        // that defines the body. That scope contains function parameters and also local variables
        // that need to be closed over.
        self.close_upvalues(self.vm.frame().slot)?;
        let frame = self.vm.frames_pop();
        if self.vm.frames.is_empty() {
            // Have reach the end of the script if there's no stack frame left.
            self.vm.stack_pop();
            return Ok(true);
        }
        // Pop all data related to the stack frame.
        self.vm.stack_remove_top(self.vm.stack.len() - frame.slot);
        // Put the function result on the stack.
        self.vm.stack_push(result)?;
        Ok(false)
    }

    fn call(&mut self) -> Result<(), RuntimeError> {
        let argc = self.read_byte()?;
        let v = self.vm.stack_top(argc as usize);
        self.call_value(*v, argc)?;
        Ok(())
    }

    fn call_value(&mut self, callee: Value, argc: u8) -> Result<(), RuntimeError> {
        match callee {
            Value::Object(o) => self.call_object(o, argc),
            _ => Err(RuntimeError::Value(ValueError::BadOperand(
                "Can only call functions and classes.",
            ))),
        }
    }

    fn call_object(&mut self, callee: ObjectRef, argc: u8) -> Result<(), RuntimeError> {
        match &callee.content {
            ObjectContent::Closure(_) => self.call_closure(callee, argc),
            ObjectContent::NativeFun(f) => self.call_native(f, argc),
            _ => Err(RuntimeError::Value(ValueError::BadOperand(
                "Can only call functions and classes.",
            ))),
        }
    }

    fn call_closure(&mut self, callee: ObjectRef, argc: u8) -> Result<(), RuntimeError> {
        let closure_ref = callee.as_closure()?;
        let fun_ref = closure_ref.fun();

        let arity = fun_ref.arity;
        if argc != arity {
            return Err(RuntimeError::BadArgumentsCount { arity, argc });
        }
        let frame = CallFrame {
            closure: callee,
            ip: 0,
            slot: self.vm.stack.len() - argc as usize - 1,
        };
        self.vm.frames_push(frame)?;
        Ok(())
    }

    fn call_native(&mut self, fun: &NativeFun, argc: u8) -> Result<(), RuntimeError> {
        if argc != fun.arity {
            return Err(RuntimeError::BadArgumentsCount {
                arity: fun.arity,
                argc,
            });
        }
        let sp = self.vm.stack.len();
        let argc = argc as usize;
        let call = fun.call;
        let res = call(&self.vm.stack[sp - argc..]);
        self.vm.stack_remove_top(argc + 1);
        self.vm.stack_push(res)?;
        Ok(())
    }

    fn jump(&mut self, direction: JumpDirection) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let frame = self.vm.frame_mut();
        match direction {
            JumpDirection::Forward => frame.ip += offset as usize,
            JumpDirection::Backward => frame.ip -= offset as usize,
        }
        Ok(())
    }

    fn jump_if_true(&mut self) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let val = self.vm.stack_top(0);
        if val.is_truthy() {
            let frame = self.vm.frame_mut();
            frame.ip += offset as usize;
        }
        Ok(())
    }

    fn jump_if_false(&mut self) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let val = self.vm.stack_top(0);
        if val.is_falsey() {
            let frame = self.vm.frame_mut();
            frame.ip += offset as usize;
        }
        Ok(())
    }

    /// Get a local variable.
    fn get_local(&mut self) -> Result<(), RuntimeError> {
        let slot = self.read_byte()? as usize;
        let frame_slot = {
            let frame = self.vm.frame();
            frame.slot
        };
        self.vm.stack_push(self.vm.stack[frame_slot + slot])?;
        Ok(())
    }

    /// Set a local variable.
    fn set_local(&mut self) -> Result<(), RuntimeError> {
        let slot = self.read_byte()? as usize;
        let frame_slot = {
            let frame = self.vm.frame();
            frame.slot
        };
        self.vm.stack[frame_slot + slot] = *self.vm.stack_top(0);
        Ok(())
    }

    /// Get a global variable or return a runtime error if it was not found.
    fn get_global(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        let object = constant.as_object()?;
        let name = object.as_string()?;
        let value = self
            .vm
            .globals
            .get(&name)
            .ok_or_else(|| RuntimeError::UndefinedVariable(name.to_string()))?;
        self.vm.stack_push(*value)?;
        Ok(())
    }

    /// Set a global variable or return a runtime error if it was not found.
    fn set_global(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        let object = constant.as_object()?;
        let name = object.as_string()?;
        let value = self.vm.stack_top(0);
        if !self.vm.globals.contains_key(&name) {
            return Err(RuntimeError::UndefinedVariable(name.to_string()));
        }
        self.vm.globals.insert(name, *value);
        Ok(())
    }

    /// Declare a variable with some initial value.
    fn defined_global(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        let global_name = constant.as_object()?.as_string()?;
        let global_value = self.vm.stack_pop();
        self.vm.globals.insert(global_name, global_value);
        Ok(())
    }

    /// Read the constant id from the next byte and load the constant with the found id.
    fn constant(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        self.vm.stack_push(constant)?;
        Ok(())
    }

    fn ne(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).ne(&rhs));
        Ok(())
    }

    fn eq(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).eq(&rhs));
        Ok(())
    }

    fn gt(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).gt(&rhs)?);
        Ok(())
    }

    fn ge(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).ge(&rhs)?);
        Ok(())
    }

    fn lt(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).lt(&rhs)?);
        Ok(())
    }

    fn le(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).le(&rhs)?);
        Ok(())
    }

    fn add(&mut self) -> Result<(), RuntimeError> {
        // The peek the first 2 items on the stack instead of pop so the GC can see them and won't
        // deaalocate the objects when we allocate a new object for the result.
        let rhs = self.vm.stack_top(0);
        let lhs = self.vm.stack_top(1);
        let res = match (*lhs, rhs) {
            // Operations on objects might allocate a new one.
            (Value::Object(o1), Value::Object(o2)) => match (&o1.content, &o2.content) {
                (ObjectContent::String(s1), ObjectContent::String(s2)) => {
                    let mut s = String::with_capacity(s1.len() + s1.len());
                    s.push_str(s1.as_ref());
                    s.push_str(s2.as_ref());
                    Value::Object(self.vm.alloc_string(s))
                }
                _ => {
                    return Err(RuntimeError::Value(ValueError::BadOperand(
                        "Operands must be two numbers or two strings.",
                    )))
                }
            },
            // Non-objects can used the `ops::Add` implementation for `Value`
            _ => lhs.add(rhs)?,
        };
        // Remove the RHS.
        self.vm.stack_pop();
        // Update the LHS.
        *self.vm.stack_top_mut(0) = res;
        Ok(())
    }

    fn sub(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.sub(&rhs)?;
        Ok(())
    }

    fn mul(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.mul(&rhs)?;
        Ok(())
    }

    fn div(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.div(&rhs)?;
        Ok(())
    }

    fn not(&mut self) -> Result<(), RuntimeError> {
        let v = self.vm.stack_top_mut(0);
        *v = v.not();
        Ok(())
    }

    fn neg(&mut self) -> Result<(), RuntimeError> {
        let v = self.vm.stack_top_mut(0);
        *v = v.neg()?;
        Ok(())
    }

    fn print(&mut self) -> Result<(), RuntimeError> {
        let val = self.vm.stack_pop();
        println!("{val}");
        Ok(())
    }
}

struct CallFrame {
    closure: ObjectRef,
    ip: usize,
    slot: usize,
}

impl CallFrame {
    fn closure(&self) -> &ObjClosure {
        self.closure.as_closure().expect("Expect closure object.")
    }

    fn closure_mut(&mut self) -> &mut ObjClosure {
        self.closure
            .as_closure_mut()
            .expect("Expect closure object.")
    }

    /// Read the next byte in the stream of bytecode instructions.
    fn read_byte(&mut self) -> Result<u8, RuntimeError> {
        let byte = {
            let closure_ref = self.closure();
            let fun_ref = closure_ref.fun();
            fun_ref.chunk.instructions[self.ip]
        };
        self.ip += 1;
        Ok(byte)
    }

    /// Read the next 2 bytes in the stream of bytecode instructions.
    fn read_short(&mut self) -> Result<u16, RuntimeError> {
        let short = {
            let closure_ref = self.closure();
            let fun_ref = closure_ref.fun();
            let hi = fun_ref.chunk.instructions[self.ip] as u16;
            let lo = fun_ref.chunk.instructions[self.ip + 1] as u16;
            hi << 8 | lo
        };
        self.ip += 2;
        Ok(short)
    }

    /// Read the next byte in the stream of bytecode instructions and return the constant at the
    /// index given by the byte.
    fn read_constant(&mut self) -> Result<Value, RuntimeError> {
        let constant_id = self.read_byte()? as usize;
        let closure_ref = self.closure();
        let fun_ref = closure_ref.fun();
        Ok(fun_ref.chunk.constants[constant_id])
    }
}

/// An enumeration that determine whether to jump forward or backward along the stream of
/// bytecode instructions.
pub(crate) enum JumpDirection {
    /// Jump forward.
    Forward,
    /// Jump backward.
    Backward,
}
