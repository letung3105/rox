use std::{collections::HashSet, fmt, ops, ptr::NonNull, rc::Rc};

use crate::{chunk::Chunk, value::Value};

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ObjectError {
    #[error("Invalid cast.")]
    InvalidCast,
}

/// A reference to the heap-allocated object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ObjectRef(pub(crate) NonNull<Object>);

impl ObjectRef {
    pub(crate) fn mark(
        &mut self,
        grey_objects: &mut Vec<ObjectRef>,
        black_objects: &HashSet<NonNull<Object>>,
    ) {
        if black_objects.contains(&self.0) {
            return;
        }

        #[cfg(feature = "dbg-heap")]
        println!("{:p} mark {self}", self.0);

        grey_objects.push(*self);
    }

    pub(crate) fn mark_references(
        &mut self,
        grey_objects: &mut Vec<ObjectRef>,
        black_objects: &HashSet<NonNull<Object>>,
    ) {
        match &mut self.content {
            ObjectContent::Upvalue(upvalue) => upvalue.mark_references(grey_objects, black_objects),
            ObjectContent::Closure(closure) => closure.mark_references(grey_objects, black_objects),
            ObjectContent::Fun(fun) => fun.mark_references(grey_objects, black_objects),
            ObjectContent::String(_) | ObjectContent::NativeFun(_) => {}
        }
    }
}

impl From<NonNull<Object>> for ObjectRef {
    fn from(value: NonNull<Object>) -> Self {
        Self(value)
    }
}

impl From<Box<Object>> for ObjectRef {
    fn from(value: Box<Object>) -> Self {
        let object_ptr = NonNull::from(Box::leak(value));
        Self::from(object_ptr)
    }
}

impl ops::Deref for ObjectRef {
    type Target = Object;

    #[allow(unsafe_code)]
    fn deref(&self) -> &Self::Target {
        // SAFETY: If we still own a handle then the underlying must not be deallocated. Thus, it's
        // safe to get a reference from the raw pointer.
        unsafe { self.0.as_ref() }
    }
}

impl ops::DerefMut for ObjectRef {
    #[allow(unsafe_code)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: If we still own a handle then the underlying must not be deallocated. Thus, it's
        // safe to get a reference from the raw pointer.
        unsafe { self.0.as_mut() }
    }
}

impl PartialEq for ObjectRef {
    fn eq(&self, other: &Self) -> bool {
        match (&self.content, &other.content) {
            (ObjectContent::String(s1), ObjectContent::String(s2)) => Rc::ptr_eq(s1, s2),
            (ObjectContent::Closure(_), ObjectContent::Closure(_)) => self.0.eq(&other.0),
            (ObjectContent::Fun(_), ObjectContent::Fun(_)) => self.0.eq(&other.0),
            (ObjectContent::NativeFun(_), ObjectContent::NativeFun(_)) => self.0.eq(&other.0),
            _ => false,
        }
    }
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "{}", self.content)
    }
}

/// The structure of a heap-allocated object.
#[derive(Debug)]
pub(crate) struct Object {
    /// A pointer to the next object in the linked list of allocated objects.
    pub(crate) next: Option<ObjectRef>,
    /// The object's data.
    pub(crate) content: ObjectContent,
}

impl Object {
    pub(crate) fn new(next: Option<ObjectRef>, content: ObjectContent) -> Self {
        Self { next, content }
    }

    /// Cast the object to a string reference.
    pub(crate) fn as_string(&self) -> Result<Rc<str>, ObjectError> {
        match &self.content {
            ObjectContent::String(s) => Ok(Rc::clone(s)),
            _ => Err(ObjectError::InvalidCast),
        }
    }

    /// Cast the object to a upvalue reference.
    pub(crate) fn as_upvalue(&self) -> Result<&ObjUpvalue, ObjectError> {
        match &self.content {
            ObjectContent::Upvalue(u) => Ok(u),
            _ => Err(ObjectError::InvalidCast),
        }
    }

    /// Cast the object to a upvalue reference.
    pub(crate) fn as_upvalue_mut(&mut self) -> Result<&mut ObjUpvalue, ObjectError> {
        match &mut self.content {
            ObjectContent::Upvalue(u) => Ok(u),
            _ => Err(ObjectError::InvalidCast),
        }
    }

    /// Cast the object to a closure reference.
    pub(crate) fn as_closure(&self) -> Result<&ObjClosure, ObjectError> {
        match &self.content {
            ObjectContent::Closure(c) => Ok(c),
            _ => Err(ObjectError::InvalidCast),
        }
    }

    /// Cast the object to a closure reference.
    pub(crate) fn as_closure_mut(&mut self) -> Result<&mut ObjClosure, ObjectError> {
        match &mut self.content {
            ObjectContent::Closure(c) => Ok(c),
            _ => Err(ObjectError::InvalidCast),
        }
    }

    /// Cast the object to a function reference.
    pub(crate) fn as_fun(&self) -> Result<&ObjFun, ObjectError> {
        match &self.content {
            ObjectContent::Fun(f) => Ok(f),
            _ => Err(ObjectError::InvalidCast),
        }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "{}", self.content)
    }
}

/// A enumeration of all supported object types in Lox and their underlying value.
#[derive(Debug)]
pub(crate) enum ObjectContent {
    /// A heap allocated string
    String(Rc<str>),
    /// A heap allocated value hoisted from the stack.
    Upvalue(ObjUpvalue),
    /// A closure that can captured surrounding variables
    Closure(ObjClosure),
    /// A function object
    Fun(ObjFun),
    /// A native function object
    NativeFun(NativeFun),
    // /// A class object
    // Class(Gc<RefCell<ObjClass>>),
    // /// A class instance
    // Instance(Gc<RefCell<ObjInstance>>),
    // /// A class instance
    // BoundMethod(Gc<ObjBoundMethod>),
}

impl fmt::Display for ObjectContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        match self {
            Self::String(s) => write!(f, "{s}"),
            Self::Upvalue(u) => write!(f, "{u}"),
            Self::Closure(c) => write!(f, "{c}"),
            Self::Fun(fun) => write!(f, "{fun}"),
            Self::NativeFun(fun) => write!(f, "{fun}"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ObjClosure {
    // The function definition of this closure.
    pub(crate) fun: ObjectRef,
    pub(crate) upvalues: Vec<ObjectRef>,
}

impl ObjClosure {
    /// Create a new closure object backed by the given function and upvalues.
    pub(crate) fn new(fun: ObjectRef, upvalues: Vec<ObjectRef>) -> Self {
        Self { fun, upvalues }
    }

    /// Get a reference to the function object.
    pub(crate) fn fun(&self) -> &ObjFun {
        self.fun.as_fun().expect("Expect function object.")
    }

    pub(crate) fn mark_references(
        &mut self,
        grey_objects: &mut Vec<ObjectRef>,
        black_objects: &HashSet<NonNull<Object>>,
    ) {
        self.fun.mark(grey_objects, black_objects);
        for upvalue in &mut self.upvalues {
            upvalue.mark(grey_objects, black_objects);
        }
    }
}

impl fmt::Display for ObjClosure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        let fun = self.fun.as_fun().expect("Expect function object.");
        write!(f, "{fun}")
    }
}

/// An upvalue represented variables that can be captured by a closure.
#[derive(Debug)]
pub(crate) enum ObjUpvalue {
    /// An open upvalue references a stack slot and represents a variable that has not been
    /// closed over.
    Open(usize),
    /// A closed upvalue owns a heap-allocated value and represents a variable that has been
    /// closed over.
    Closed(Value),
}

impl ObjUpvalue {
    pub(crate) fn mark_references(
        &mut self,
        grey_objects: &mut Vec<ObjectRef>,
        black_objects: &HashSet<NonNull<Object>>,
    ) {
        if let ObjUpvalue::Closed(Value::Object(obj)) = self {
            obj.mark(grey_objects, black_objects);
        }
    }
}

impl fmt::Display for ObjUpvalue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "upvalue")
    }
}

#[derive(Debug)]
pub(crate) struct ObjFun {
    /// The name of the function
    pub(crate) name: Option<Rc<str>>,
    /// Number of parameters the function has
    pub(crate) arity: u8,
    /// Number of upvalues captured by the function
    pub(crate) upvalue_count: u8,
    /// The bytecode chunk of this function
    pub(crate) chunk: Chunk,
}

impl ObjFun {
    pub(crate) fn new(name: Option<Rc<str>>) -> Self {
        Self {
            name,
            arity: 0,
            upvalue_count: 0,
            chunk: Chunk::default(),
        }
    }

    pub(crate) fn mark_references(
        &mut self,
        grey_objects: &mut Vec<ObjectRef>,
        black_objects: &HashSet<NonNull<Object>>,
    ) {
        for constant in &self.chunk.constants {
            if let Value::Object(mut obj) = constant {
                obj.mark(grey_objects, black_objects);
            }
        }
    }
}

impl fmt::Display for ObjFun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        match &self.name {
            None => write!(f, "<script>"),
            Some(s) => write!(f, "<fn {s}>"),
        }
    }
}

/// A native function
pub(crate) struct NativeFun {
    /// Number of parameters
    pub(crate) arity: u8,
    /// Native function reference
    pub(crate) call: fn(&[Value]) -> Value,
}

impl fmt::Display for NativeFun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "<native fn>")
    }
}

impl fmt::Debug for NativeFun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "<native fn>")
    }
}
