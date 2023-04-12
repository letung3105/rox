//! Implementation of the bytecode virtual machine.

use std::{
    cell::RefCell,
    ops::{Add, Deref, DerefMut, Div, Mul, Neg, Not, Sub},
    rc::Rc,
};

use rustc_hash::FxHashMap;

use crate::{
    compile::Parser,
    heap::Heap,
    object::{
        ObjBoundMethod, ObjClass, ObjClosure, ObjFun, ObjInstance, ObjNativeFun, ObjUpvalue,
        Object, ObjectError, RefBoundMethod, RefClass, RefClosure, RefFun, RefInstance,
        RefNativeFun, RefString, RefUpvalue,
    },
    opcode::Opcode,
    stack::Stack,
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

    /// Overflow the virtual machine's stack.
    #[error("Stack overflow.")]
    StackOverflow,

    /// Can't access a property.
    #[error("Only instances have properties.")]
    ObjectHasNoProperty,

    /// Can't access a field.
    #[error("Only instances have fields.")]
    ObjectHasNoField,

    /// Can't find a variable in scope.
    #[error("Undefined variable '{0}'.")]
    UndefinedVariable(String),

    /// Can't find a property in the instance.
    #[error("Undefined property '{0}'.")]
    UndefinedProperty(String),

    /// Can't inherit objects that are not supported.
    #[error("Superclass must be a class.")]
    InvalidSuperclass,

    /// Can't call objects that are not supported.
    #[error("Can only call functions and classes.")]
    InvalidCallee,

    /// Can't invoke objects that are not supported.
    #[error("Only instances have methods.")]
    InvalidMethodInvocation,

    /// Called a function/method with incorrect number of arguments.
    #[error("Expected {arity} arguments but got {argc}.")]
    InvalidArgumentsCount {
        /// The arity of the function.
        arity: u8,
        /// The number of arguments given.
        argc: u8,
    },
}

/// A bytecode virtual machine for the Lox programming language.
pub struct VirtualMachine {
    stack: Stack<Value, VM_STACK_SIZE>,
    frames: Stack<CallFrame, VM_FRAMES_MAX>,
    open_upvalues: Vec<RefUpvalue>,
    globals: FxHashMap<Rc<str>, Value>,
    grey_objects: Vec<Object>,
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
            stack: Stack::default(),
            frames: Stack::default(),
            open_upvalues: Vec::new(),
            globals: FxHashMap::default(),
            grey_objects: Vec::new(),
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
        let (fun_object, fun_ref) = self.alloc_fun(fun);
        // Remove all added constants.
        self.stack_remove_top(constant_count);

        // Push the function onto the stack so GC won't remove it while we allocating the closure.
        self.stack_push(Value::Object(fun_object))?;
        // Create a closure for the script function. Note that script can't have upvalues.
        let (closure_object, closure_ref) = self.alloc_closure(ObjClosure {
            fun: fun_ref,
            upvalues: Vec::new(),
        });
        // Pop the fun object as we no longer need it.
        self.stack_pop();

        // Push the closure onto the stack so GC won't remove for the entire runtime.
        self.stack_push(Value::Object(closure_object))?;
        // Start running the closure.
        let mut task = Task::new(self);
        task.call_closure(closure_ref, 0).and_then(|_| task.run())
    }

    fn frame(&self) -> &CallFrame {
        self.frames.top(0)
    }

    fn frame_mut(&mut self) -> &mut CallFrame {
        self.frames.top_mut(0)
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
        self.frames.pop()
    }

    fn stack_push(&mut self, value: Value) -> Result<(), RuntimeError> {
        let stack_size = self.stack.len();
        if stack_size == VM_STACK_SIZE {
            return Err(RuntimeError::StackOverflow);
        }
        self.stack.push(value);
        Ok(())
    }

    fn stack_pop(&mut self) -> Value {
        self.stack.pop()
    }

    fn stack_top(&self, n: usize) -> &Value {
        self.stack.top(n)
    }

    fn stack_top_mut(&mut self, n: usize) -> &mut Value {
        self.stack.top_mut(n)
    }

    fn stack_remove_top(&mut self, n: usize) {
        self.stack.remove(n);
    }

    fn trace_calls(&self) -> Result<(), RuntimeError> {
        for frame in self.frames.into_iter().rev() {
            let line = frame.closure.fun.chunk.get_line(frame.ip - 1);
            match &frame.closure.fun.name {
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
        let (fun, _) = self.alloc_native_fun(ObjNativeFun { arity, call });
        self.stack_push(Value::Object(fun))?;
        self.globals.insert(fun_name, *self.stack_top(0));
        self.stack_pop();
        Ok(())
    }

    fn alloc_string(&mut self, s: String) -> (Object, RefString) {
        self.gc();
        let s = self.heap.intern(s);
        self.heap.alloc(s, Object::String)
    }

    fn alloc_upvalue(&mut self, upvalue: ObjUpvalue) -> (Object, RefUpvalue) {
        self.gc();
        self.heap.alloc(RefCell::new(upvalue), Object::Upvalue)
    }

    fn alloc_closure(&mut self, closure: ObjClosure) -> (Object, RefClosure) {
        self.gc();
        self.heap.alloc(closure, Object::Closure)
    }

    fn alloc_fun(&mut self, fun: ObjFun) -> (Object, RefFun) {
        self.gc();
        self.heap.alloc(fun, Object::Fun)
    }

    fn alloc_native_fun(&mut self, native_fun: ObjNativeFun) -> (Object, RefNativeFun) {
        self.gc();
        self.heap.alloc(native_fun, Object::NativeFun)
    }

    fn alloc_class(&mut self, class: ObjClass) -> (Object, RefClass) {
        self.gc();
        self.heap.alloc(RefCell::new(class), Object::Class)
    }

    fn alloc_instance(&mut self, instance: ObjInstance) -> (Object, RefInstance) {
        self.gc();
        self.heap.alloc(RefCell::new(instance), Object::Instance)
    }

    fn alloc_bound_method(&mut self, method: ObjBoundMethod) -> (Object, RefBoundMethod) {
        self.gc();
        self.heap.alloc(method, Object::BoundMethod)
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

    #[allow(unsafe_code)]
    fn mark_sweep(&mut self) {
        self.mark_roots();
        while let Some(grey_object) = self.grey_objects.pop() {
            grey_object.mark_references(&mut self.grey_objects)
        }
        // SAFETY: We make sure that the sweep step has correctly mark all reachable objects, so
        // sweep can be run safely.
        unsafe { self.heap.sweep() };
    }

    fn mark_roots(&mut self) {
        self.grey_objects.clear();
        for value in &self.stack {
            if let Value::Object(o) = value {
                o.mark(&mut self.grey_objects);
            }
        }
        for frame in &self.frames {
            if frame.closure.mark() {
                self.grey_objects.push(Object::Closure(frame.closure));
            }
        }
        for upvalue in &self.open_upvalues {
            if upvalue.mark() {
                self.grey_objects.push(Object::Upvalue(*upvalue));
            }
        }
        for value in self.globals.values() {
            if let Value::Object(o) = value {
                o.mark(&mut self.grey_objects);
            }
        }
    }

    #[cfg(feature = "dbg-execution")]
    fn trace_stack(&self) {
        print!("          ");
        for value in self.stack.into_iter() {
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
    fn read_byte(&mut self, instructions: &[u8]) -> Result<u8, RuntimeError> {
        let frame = self.vm.frame_mut();
        let byte = instructions[frame.ip];
        frame.ip += 1;
        Ok(byte)
    }

    /// Read the next 2 bytes in the stream of bytecode instructions.
    fn read_short(&mut self, instructions: &[u8]) -> Result<u16, RuntimeError> {
        let frame = self.vm.frame_mut();
        let hi = instructions[frame.ip] as u16;
        let lo = instructions[frame.ip + 1] as u16;
        let short = hi << 8 | lo;
        frame.ip += 2;
        Ok(short)
    }

    /// Read the next byte in the stream of bytecode instructions and return the constant at the
    /// index given by the byte.
    #[allow(unsafe_code)]
    fn read_constant(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<Value, RuntimeError> {
        let frame = self.vm.frame_mut();
        let constant_id = instructions[frame.ip];
        frame.ip += 1;
        // SAFETY: The compiler should produce correct byte codes.
        let constant = unsafe { *constants.at(constant_id as usize) };
        Ok(constant)
    }
}

impl<'vm> Task<'vm> {
    fn run(&mut self) -> Result<(), RuntimeError> {
        let mut closure = self.vm.frame().closure;
        let mut instructions = &closure.fun.chunk.instructions;
        let mut constants = &closure.fun.chunk.constants;
        loop {
            #[cfg(feature = "dbg-execution")]
            {
                self.vm.trace_stack();
                disassemble_instruction(&closure.fun.chunk, self.vm.frame().ip);
            }

            let mut is_frame_changed = false;
            match Opcode::try_from(self.read_byte(instructions)?)? {
                Opcode::Const => self.constant(instructions, constants)?,
                Opcode::Nil => self.vm.stack_push(Value::Nil)?,
                Opcode::True => self.vm.stack_push(Value::Bool(true))?,
                Opcode::False => self.vm.stack_push(Value::Bool(false))?,
                Opcode::Pop => {
                    self.vm.stack_pop();
                }
                Opcode::GetLocal => self.get_local(instructions)?,
                Opcode::SetLocal => self.set_local(instructions)?,
                Opcode::GetGlobal => self.get_global(instructions, constants)?,
                Opcode::SetGlobal => self.set_global(instructions, constants)?,
                Opcode::DefineGlobal => self.defined_global(instructions, constants)?,
                Opcode::GetUpvalue => self.get_upvalue(closure, instructions)?,
                Opcode::SetUpvalue => self.set_upvalue(closure, instructions)?,
                Opcode::GetProperty => self.get_property(instructions, constants)?,
                Opcode::SetProperty => self.set_property(instructions, constants)?,
                Opcode::GetSuper => self.get_super(instructions, constants)?,
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
                Opcode::Jump => self.jump(JumpDirection::Forward, instructions)?,
                Opcode::JumpIfTrue => self.jump_if_true(instructions)?,
                Opcode::JumpIfFalse => self.jump_if_false(instructions)?,
                Opcode::Loop => self.jump(JumpDirection::Backward, instructions)?,
                Opcode::Call => {
                    self.call(instructions)?;
                    is_frame_changed = true;
                }
                Opcode::Invoke => {
                    self.invoke(instructions, constants)?;
                    is_frame_changed = true;
                }
                Opcode::SuperInvoke => {
                    self.super_invoke(instructions, constants)?;
                    is_frame_changed = true;
                }
                Opcode::Closure => {
                    self.closure(closure, instructions, constants)?;
                    is_frame_changed = true;
                }
                Opcode::CloseUpvalue => self.close_upvalue()?,
                Opcode::Ret => {
                    if self.ret()? {
                        break;
                    }
                    is_frame_changed = true;
                }
                Opcode::Class => self.class(instructions, constants)?,
                Opcode::Inherit => self.inherit()?,
                Opcode::Method => self.method(instructions, constants)?,
            }
            if is_frame_changed {
                closure = self.vm.frame().closure;
                instructions = &closure.fun.chunk.instructions;
                constants = &closure.fun.chunk.constants;
            }
        }
        Ok(())
    }

    fn super_invoke(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let method = self.read_constant(instructions, constants)?.as_string()?;
        let argc = self.read_byte(instructions)?;

        let superclass = self.vm.stack_pop().as_class()?;
        self.invoke_from_class(superclass, &method, argc)?;
        Ok(())
    }

    fn invoke(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let method = self.read_constant(instructions, constants)?.as_string()?;
        let argc = self.read_byte(instructions)?;

        let receiver = self.vm.stack_top(argc as usize);
        let instance = receiver
            .as_instance()
            .map_err(|_| RuntimeError::InvalidMethodInvocation)?;

        if let Some(field) = instance.borrow().fields.get(&***method) {
            *self.vm.stack_top_mut(argc as usize) = *field;
            self.call_value(*field, argc)?;
        } else {
            self.invoke_from_class(instance.borrow().class, &method, argc)?;
        }

        Ok(())
    }

    fn invoke_from_class(
        &mut self,
        class: RefClass,
        name: &str,
        argc: u8,
    ) -> Result<(), RuntimeError> {
        let method = class
            .borrow()
            .methods
            .get(name)
            .copied()
            .ok_or_else(|| RuntimeError::UndefinedProperty(name.to_string()))?;
        self.call_closure(method, argc)?;
        Ok(())
    }

    // Bind a method to a class definition. At this moment, a closure object should be the top most
    // item in the stack, and a class definition object should be the second top most item.
    fn method(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let closure = self.vm.stack_pop().as_closure()?;
        let class = self.vm.stack_top(0).as_class()?;
        class.borrow_mut().methods.insert(Rc::clone(&name), closure);
        Ok(())
    }

    fn bind_method(&mut self, class: RefClass, name: &str) -> Result<bool, RuntimeError> {
        match class.borrow().methods.get(name) {
            Some(method) => {
                let (bound, _) = self.vm.alloc_bound_method(ObjBoundMethod {
                    receiver: *self.vm.stack_top(0),
                    method: *method,
                });
                self.vm.stack_pop();
                self.vm.stack_push(Value::Object(bound))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn get_property(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let instance = self
            .vm
            .stack_top(0)
            .as_instance()
            .map_err(|_| RuntimeError::ObjectHasNoProperty)?;

        let instance = instance.borrow();
        if let Some(value) = instance.fields.get(&***name) {
            self.vm.stack_pop();
            self.vm.stack_push(*value)?;
            Ok(())
        } else if self.bind_method(instance.class, &name)? {
            Ok(())
        } else {
            Err(RuntimeError::UndefinedProperty(name.to_string()))
        }
    }

    fn set_property(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let value = self.vm.stack_pop();
        let instance = self
            .vm
            .stack_top(0)
            .as_instance()
            .map_err(|_| RuntimeError::ObjectHasNoField)?;

        instance.borrow_mut().fields.insert(Rc::clone(&name), value);
        self.vm.stack_pop();
        self.vm.stack_push(value)?;
        Ok(())
    }

    fn get_super(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let superclass = self.vm.stack_pop().as_class()?;
        if !self.bind_method(superclass, &name)? {
            return Err(RuntimeError::UndefinedProperty(name.to_string()));
        }
        Ok(())
    }

    fn class(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let (class, _) = self.vm.alloc_class(ObjClass::new(Rc::clone(&name)));
        self.vm.stack_push(Value::Object(class))?;
        Ok(())
    }

    fn inherit(&mut self) -> Result<(), RuntimeError> {
        let superclass = self
            .vm
            .stack_top(1)
            .as_class()
            .map_err(|_| RuntimeError::InvalidSuperclass)?;
        let subclass = self.vm.stack_top(0).as_class()?;
        for (method_name, method) in &superclass.borrow().methods {
            subclass
                .borrow_mut()
                .methods
                .insert(Rc::clone(method_name), *method);
        }
        self.vm.stack_pop();
        Ok(())
    }

    /// Get the value of the variable capture by an upvalue.
    #[allow(unsafe_code)]
    fn get_upvalue(
        &mut self,
        closure: RefClosure,
        instructions: &[u8],
    ) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte(instructions)?;
        let upvalue = closure.upvalues[upvalue_slot as usize];
        match *upvalue.borrow() {
            // Value is on the stack.
            ObjUpvalue::Open(stack_slot) => {
                // SAFETY: The compiler should produce safe byte codes such that we never
                // access uninitialized data.
                let value = unsafe { self.vm.stack.at(stack_slot) };
                self.vm.stack_push(*value)?;
            }
            // Value is on the heap.
            ObjUpvalue::Closed(value) => {
                self.vm.stack_push(value)?;
            }
        }
        Ok(())
    }

    /// Set the value of the variable capture by an upvalue.
    #[allow(unsafe_code)]
    fn set_upvalue(
        &mut self,
        closure: RefClosure,
        instructions: &[u8],
    ) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte(instructions)?;
        let value = *self.vm.stack_top(0);
        let stack_slot = {
            let mut upvalue = closure.upvalues[upvalue_slot as usize].borrow_mut();
            match upvalue.deref_mut() {
                // Value is on the stack.
                ObjUpvalue::Open(stack_slot) => Some(*stack_slot),
                // Value is on the heap.
                ObjUpvalue::Closed(v) => {
                    *v = value;
                    None
                }
            }
        };
        if let Some(slot) = stack_slot {
            // SAFETY: The compiler should produce safe code that access a safe part of the stack.
            let v = unsafe { self.vm.stack.at_mut(slot) };
            *v = value;
        }
        Ok(())
    }

    fn closure(
        &mut self,
        closure: RefClosure,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let fun = self.read_constant(instructions, constants)?.as_fun()?;
        let mut upvalues = Vec::with_capacity(fun.upvalue_count as usize);
        for _ in 0..fun.upvalue_count {
            let is_local = self.read_byte(instructions)? == 1;
            let index = self.read_byte(instructions)? as usize;
            if is_local {
                upvalues.push(self.capture_upvalue(self.vm.frame().slot + index)?);
            } else {
                upvalues.push(closure.upvalues[index]);
            }
        }

        let (closure, _) = self.vm.alloc_closure(ObjClosure { fun, upvalues });
        self.vm.stack_push(Value::Object(closure))?;

        Ok(())
    }

    fn close_upvalue(&mut self) -> Result<(), RuntimeError> {
        self.close_upvalues(self.vm.stack.len() - 1)?;
        self.vm.stack_pop();
        Ok(())
    }

    /// Create an open upvalue capturing the variable at the stack index given by `location`.
    fn capture_upvalue(&mut self, location: usize) -> Result<RefUpvalue, RuntimeError> {
        // Check if there's an existing open upvalues that references the same stack slot. We want
        // to close over a variable not a value. Thus, closures can shared data through the same
        // captured variable.
        for upvalue in self.vm.open_upvalues.iter() {
            if let ObjUpvalue::Open(loc) = *upvalue.borrow() {
                if loc == location {
                    return Ok(*upvalue);
                }
            }
        }
        // Make a new open upvalue.
        let (_, upvalue_ref) = self.vm.alloc_upvalue(ObjUpvalue::Open(location));
        self.vm.open_upvalues.push(upvalue_ref);
        Ok(upvalue_ref)
    }

    // Close all upvalues whose referenced stack slot went out of scope. Here, `last` is the lowest
    // stack slot that went out of scope.
    #[allow(unsafe_code)]
    fn close_upvalues(&mut self, last: usize) -> Result<(), RuntimeError> {
        for upvalue in &self.vm.open_upvalues {
            // Check if we reference a slot that went out of scope.
            let mut upvalue = upvalue.borrow_mut();
            let stack_slot = match *upvalue {
                ObjUpvalue::Open(slot) if slot >= last => Some(slot),
                _ => None,
            };
            // Hoist the variable up into the upvalue so it can live after the stack frame is pop.
            if let Some(slot) = stack_slot {
                // SAFETY: The compiler should produce safe code that access a safe part of the stack.
                let v = unsafe { self.vm.stack.at(slot) };
                *upvalue = ObjUpvalue::Closed(*v);
            }
        }
        // remove closed upvalues from list of open upvalues
        self.vm
            .open_upvalues
            .retain(|v| matches!(v.borrow().deref(), ObjUpvalue::Open(_)));
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
        if self.vm.frames.len() == 0 {
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

    fn call(&mut self, instructions: &[u8]) -> Result<(), RuntimeError> {
        let argc = self.read_byte(instructions)?;
        let v = self.vm.stack_top(argc as usize);
        self.call_value(*v, argc)?;
        Ok(())
    }

    fn call_value(&mut self, callee: Value, argc: u8) -> Result<(), RuntimeError> {
        match callee {
            Value::Object(o) => self.call_object(o, argc),
            _ => Err(RuntimeError::InvalidCallee),
        }
    }

    fn call_object(&mut self, callee: Object, argc: u8) -> Result<(), RuntimeError> {
        match &callee {
            Object::Closure(c) => self.call_closure(*c, argc),
            Object::NativeFun(f) => self.call_native(*f, argc),
            Object::Class(c) => self.call_class(*c, argc),
            Object::BoundMethod(m) => self.call_bound_method(*m, argc),
            _ => Err(RuntimeError::InvalidCallee),
        }
    }

    fn call_closure(&mut self, callee: RefClosure, argc: u8) -> Result<(), RuntimeError> {
        if argc != callee.fun.arity {
            return Err(RuntimeError::InvalidArgumentsCount {
                arity: callee.fun.arity,
                argc,
            });
        }
        let frame = CallFrame {
            closure: callee,
            ip: 0,
            slot: self.vm.stack.len() - argc as usize - 1,
        };
        self.vm.frames_push(frame)?;
        Ok(())
    }

    fn call_native(&mut self, callee: RefNativeFun, argc: u8) -> Result<(), RuntimeError> {
        if argc != callee.arity {
            return Err(RuntimeError::InvalidArgumentsCount {
                arity: callee.arity,
                argc,
            });
        }
        let argc = argc as usize;
        let call = callee.call;
        let res = call(self.vm.stack.topn(argc));
        self.vm.stack_remove_top(argc + 1);
        self.vm.stack_push(res)?;
        Ok(())
    }

    fn call_class(&mut self, callee: RefClass, argc: u8) -> Result<(), RuntimeError> {
        // Allocate a new instance and put it on top of the stack.
        let (instance, _) = self.vm.alloc_instance(ObjInstance::new(callee));
        *self.vm.stack_top_mut(argc.into()) = Value::Object(instance);
        // Call the 'init' method if there's one
        if let Some(init) = callee.borrow().methods.get("init") {
            self.call_closure(*init, argc)?;
        } else if argc != 0 {
            return Err(RuntimeError::InvalidArgumentsCount { arity: 0, argc });
        }
        Ok(())
    }

    fn call_bound_method(&mut self, callee: RefBoundMethod, argc: u8) -> Result<(), RuntimeError> {
        *self.vm.stack_top_mut(argc as usize) = callee.receiver;
        self.call_closure(callee.method, argc)?;
        Ok(())
    }

    fn jump(&mut self, direction: JumpDirection, instructions: &[u8]) -> Result<(), RuntimeError> {
        let offset = self.read_short(instructions)?;
        let frame = self.vm.frame_mut();
        match direction {
            JumpDirection::Forward => frame.ip += offset as usize,
            JumpDirection::Backward => frame.ip -= offset as usize,
        }
        Ok(())
    }

    fn jump_if_true(&mut self, instructions: &[u8]) -> Result<(), RuntimeError> {
        let offset = self.read_short(instructions)?;
        let val = self.vm.stack_top(0);
        if val.is_truthy() {
            self.vm.frame_mut().ip += offset as usize;
        }
        Ok(())
    }

    fn jump_if_false(&mut self, instructions: &[u8]) -> Result<(), RuntimeError> {
        let offset = self.read_short(instructions)?;
        let val = self.vm.stack_top(0);
        if val.is_falsey() {
            self.vm.frame_mut().ip += offset as usize;
        }
        Ok(())
    }

    /// Get a local variable.
    #[allow(unsafe_code)]
    fn get_local(&mut self, instructions: &[u8]) -> Result<(), RuntimeError> {
        let slot = self.read_byte(instructions)? as usize;
        let frame_slot = self.vm.frame().slot;
        // SAFETY: The compiler should produce safe code that access a safe part of the stack.
        let value = unsafe { self.vm.stack.at(frame_slot + slot) };
        self.vm.stack_push(*value)?;
        Ok(())
    }

    /// Set a local variable.
    #[allow(unsafe_code)]
    fn set_local(&mut self, instructions: &[u8]) -> Result<(), RuntimeError> {
        let slot = self.read_byte(instructions)? as usize;
        let frame_slot = self.vm.frame().slot;
        let value = *self.vm.stack_top(0);
        // SAFETY: The compiler should produce safe code that access a safe part of the stack.
        let v = unsafe { self.vm.stack.at_mut(frame_slot + slot) };
        *v = value;
        Ok(())
    }

    /// Get a global variable or return a runtime error if it was not found.
    fn get_global(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let value = self
            .vm
            .globals
            .get(&***name)
            .ok_or_else(|| RuntimeError::UndefinedVariable(name.to_string()))?;
        self.vm.stack_push(*value)?;
        Ok(())
    }

    /// Set a global variable or return a runtime error if it was not found.
    fn set_global(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let value = self.vm.stack_top(0);
        if !self.vm.globals.contains_key(&***name) {
            return Err(RuntimeError::UndefinedVariable(name.to_string()));
        }
        self.vm.globals.insert(Rc::clone(&name), *value);
        Ok(())
    }

    /// Declare a variable with some initial value.
    fn defined_global(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let name = self.read_constant(instructions, constants)?.as_string()?;
        let value = self.vm.stack_pop();
        self.vm.globals.insert(Rc::clone(&name), value);
        Ok(())
    }

    /// Read the constant id from the next byte and load the constant with the found id.
    fn constant(
        &mut self,
        instructions: &[u8],
        constants: &Stack<Value, VM_STACK_SIZE>,
    ) -> Result<(), RuntimeError> {
        let constant = self.read_constant(instructions, constants)?;
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
            (Value::Object(o1), Value::Object(o2)) => match (o1, o2) {
                (Object::String(s1), Object::String(s2)) => {
                    let mut s = String::with_capacity(s1.len() + s1.len());
                    s.push_str(s1.as_ref());
                    s.push_str(s2.as_ref());
                    let (object, _) = self.vm.alloc_string(s);
                    Value::Object(object)
                }
                _ => {
                    return Err(RuntimeError::Value(
                        ValueError::BinaryOperandsMustBeNumbersOrStrings,
                    ))
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

#[derive(Debug)]
struct CallFrame {
    closure: RefClosure,
    ip: usize,
    slot: usize,
}

/// An enumeration that determine whether to jump forward or backward along the stream of
/// bytecode instructions.
pub(crate) enum JumpDirection {
    /// Jump forward.
    Forward,
    /// Jump backward.
    Backward,
}
