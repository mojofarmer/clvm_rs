use crate::err_utils::err;
use crate::number::{node_from_number, number_from_u8, Number};
use crate::reduction::EvalErr;
use chia_bls::{G1Element, G2Element};
use std::cell::RefCell;

const MAX_NUM_ATOMS: usize = 62500000;
const MAX_NUM_PAIRS: usize = 62500000;
const NODE_PTR_IDX_BITS: u32 = 26;
const NODE_PTR_IDX_MASK: u32 = (1 << NODE_PTR_IDX_BITS) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodePtr(u32);

enum ObjectType {
    // The low bits form an index into the pair_vec
    Pair,
    // The low bits form an index into the atom_vec
    Bytes,
    // The low bits are the atom itself (unsigned integer, 26 bits)
    SmallAtom,
}

// The top 6 bits of the NodePtr indicate what type of object it is
impl NodePtr {
    pub const NIL: Self = Self::new(ObjectType::SmallAtom, 0);

    const fn new(t: ObjectType, idx: usize) -> Self {
        debug_assert!(idx <= NODE_PTR_IDX_MASK as usize);
        NodePtr(((t as u32) << NODE_PTR_IDX_BITS) | (idx as u32))
    }

    fn node_type(&self) -> (ObjectType, u32) {
        (
            match self.0 >> NODE_PTR_IDX_BITS {
                0 => ObjectType::Pair,
                1 => ObjectType::Bytes,
                2 => ObjectType::SmallAtom,
                _ => {
                    panic!("unknown NodePtr type");
                }
            },
            (self.0 & NODE_PTR_IDX_MASK),
        )
    }

    pub(crate) fn as_index(&self) -> usize {
        match self.node_type() {
            (ObjectType::Pair, idx) => (idx as usize) * 3,
            (ObjectType::Bytes, idx) => (idx as usize) * 3 + 1,
            (ObjectType::SmallAtom, idx) => (idx as usize) * 3 + 2,
        }
    }
}

impl Default for NodePtr {
    fn default() -> Self {
        Self::NIL
    }
}

#[derive(PartialEq, Debug)]
pub enum SExp {
    Atom,
    Pair(NodePtr, NodePtr),
}

#[derive(Clone, Copy, Debug)]
struct AtomBuf {
    start: u32,
    end: u32,
}

impl AtomBuf {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IntPair {
    first: NodePtr,
    rest: NodePtr,
}

// this represents a specific (former) state of an allocator. This can be used
// to restore an allocator to a previous state. It cannot be used to re-create
// the state from some other allocator.
pub struct Checkpoint {
    u8s: usize,
    pairs: usize,
    atoms: usize,
    small_atoms: usize,
}

pub enum NodeVisitor<'a> {
    Buffer(&'a [u8]),
    U32(u32),
    Pair(NodePtr, NodePtr),
}

#[derive(Debug)]
pub struct Allocator {
    // this is effectively a grow-only stack where atoms are allocated. Atoms
    // are immutable, so once they are created, they will stay around until the
    // program completes
    u8_vec: Vec<u8>,

    // storage for all pairs (positive indices)
    pair_vec: Vec<IntPair>,

    // storage for all atoms (negative indices).
    // node index -1 refers to index 0 in this vector, -2 refers to 1 and so
    // on.
    atom_vec: Vec<AtomBuf>,

    // index into temp_buf array
    temp_idx: RefCell<usize>,

    // temporary buffers for storing SmallAtoms in to return from atom()
    temp_vec: Vec<RefCell<[u8; 4]>>,

    // the atom_vec may not grow past this
    heap_limit: usize,

    // the number of small atoms we've allocated. We keep track of these to ensure the limit on the
    // number of atoms is identical to what it was before the small-atom optimization
    small_atoms: usize,
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

pub fn canonical_positive_integer(v: &[u8]) -> bool {
    if v.is_empty() {
        // empty buffer is 0/nil
        true
    } else if (v.len() == 1 && v[0] == 0)
        // a 1-byte buffer of 0 is not the canonical representation of 0
        || (v[0] & 0x80) != 0
        // if the top bit is set, it's a negative number (i.e. not positive)
        || (v[0] == 0 && (v[1] & 0x80) == 0)
    {
        // if the top byte is a 0 but the top bit of the next byte is not set, that's a redundant
        // leading zero. i.e. not canonical representation
        false
    } else {
        true
    }
}

pub fn len_for_value(val: u32) -> usize {
    if val == 0 {
        0
    } else if val < 0x80 {
        1
    } else if val < 0x8000 {
        2
    } else if val < 0x800000 {
        3
    } else if val < 0x80000000 {
        4
    } else {
        5
    }
}

impl Allocator {
    pub fn new() -> Self {
        Self::new_limited(u32::MAX as usize)
    }

    pub fn new_limited(heap_limit: usize) -> Self {
        // we have a maximum of 4 GiB heap, because pointers are 32 bit unsigned
        assert!(heap_limit <= u32::MAX as usize);

        let mut temp_vec = Vec::<RefCell<[u8; 4]>>::with_capacity(64);
        for _ in 0..16 {
            temp_vec.push(RefCell::default());
        }

        let mut r = Self {
            u8_vec: Vec::new(),
            pair_vec: Vec::new(),
            atom_vec: Vec::new(),
            temp_idx: RefCell::new(0),
            temp_vec,
            // subtract 1 to compensate for the one() we used to allocate unconfitionally
            heap_limit: heap_limit - 1,
            // initialize this to 2 to behave as if we had allocated atoms for
            // nil() and one(), like we used to
            small_atoms: 2,
        };
        r.u8_vec.reserve(1024 * 1024);
        r.atom_vec.reserve(256);
        r.pair_vec.reserve(256);
        r
    }

    // create a checkpoint for the current state of the allocator. This can be
    // used to go back to an earlier allocator state by passing the Checkpoint
    // to restore_checkpoint().
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            u8s: self.u8_vec.len(),
            pairs: self.pair_vec.len(),
            atoms: self.atom_vec.len(),
            small_atoms: self.small_atoms,
        }
    }

    pub fn restore_checkpoint(&mut self, cp: &Checkpoint) {
        // if any of these asserts fire, it means we're trying to restore to
        // a state that has already been "long-jumped" passed (via another
        // restore to an earler state). You can only restore backwards in time,
        // not forwards.
        assert!(self.u8_vec.len() >= cp.u8s);
        assert!(self.pair_vec.len() >= cp.pairs);
        assert!(self.atom_vec.len() >= cp.atoms);
        self.u8_vec.truncate(cp.u8s);
        self.pair_vec.truncate(cp.pairs);
        self.atom_vec.truncate(cp.atoms);
        self.small_atoms = cp.small_atoms;
    }

    pub fn new_atom(&mut self, v: &[u8]) -> Result<NodePtr, EvalErr> {
        let start = self.u8_vec.len() as u32;
        if (self.heap_limit - start as usize) < v.len() {
            return err(self.nil(), "out of memory");
        }
        let idx = self.atom_vec.len();
        self.check_atom_limit()?;
        if v.len() <= 3 && canonical_positive_integer(v) {
            let mut ret: u32 = 0;
            for b in v {
                ret <<= 8;
                ret |= *b as u32;
            }
            self.small_atoms += 1;
            Ok(NodePtr::new(ObjectType::SmallAtom, ret as usize))
        } else {
            self.u8_vec.extend_from_slice(v);
            let end = self.u8_vec.len() as u32;
            self.atom_vec.push(AtomBuf { start, end });
            Ok(NodePtr::new(ObjectType::Bytes, idx))
        }
    }

    pub fn new_small_number(&mut self, v: u32) -> Result<NodePtr, EvalErr> {
        debug_assert!(v <= NODE_PTR_IDX_MASK);
        self.check_atom_limit()?;
        self.small_atoms += 1;
        Ok(NodePtr::new(ObjectType::SmallAtom, v as usize))
    }

    pub fn new_number(&mut self, v: Number) -> Result<NodePtr, EvalErr> {
        use num_traits::ToPrimitive;
        if let Some(val) = v.to_u32() {
            if val <= NODE_PTR_IDX_MASK {
                return self.new_small_number(val);
            }
        }
        node_from_number(self, &v)
    }

    pub fn new_g1(&mut self, g1: G1Element) -> Result<NodePtr, EvalErr> {
        self.new_atom(&g1.to_bytes())
    }

    pub fn new_g2(&mut self, g2: G2Element) -> Result<NodePtr, EvalErr> {
        self.new_atom(&g2.to_bytes())
    }

    pub fn new_pair(&mut self, first: NodePtr, rest: NodePtr) -> Result<NodePtr, EvalErr> {
        let idx = self.pair_vec.len();
        if idx == MAX_NUM_PAIRS {
            return err(self.nil(), "too many pairs");
        }
        self.pair_vec.push(IntPair { first, rest });
        Ok(NodePtr::new(ObjectType::Pair, idx))
    }

    pub fn new_substr(&mut self, node: NodePtr, start: u32, end: u32) -> Result<NodePtr, EvalErr> {
        self.check_atom_limit()?;

        fn bounds_check(node: NodePtr, start: u32, end: u32, len: u32) -> Result<(), EvalErr> {
            if start > len {
                return err(node, "substr start out of bounds");
            }
            if end > len {
                return err(node, "substr end out of bounds");
            }
            if end < start {
                return err(node, "substr invalid bounds");
            }
            Ok(())
        }

        match node.node_type() {
            (ObjectType::Pair, _) => err(node, "(internal error) substr expected atom, got pair"),
            (ObjectType::Bytes, idx) => {
                let atom = self.atom_vec[idx as usize];
                let atom_len = atom.end - atom.start;
                bounds_check(node, start, end, atom_len)?;
                let idx = self.atom_vec.len();
                self.atom_vec.push(AtomBuf {
                    start: atom.start + start,
                    end: atom.start + end,
                });
                Ok(NodePtr::new(ObjectType::Bytes, idx))
            }
            (ObjectType::SmallAtom, val) => {
                let len = len_for_value(val) as u32;
                bounds_check(node, start, end, len)?;
                let buf: [u8; 4] = val.to_be_bytes();
                let buf = &buf[4 - len as usize..];
                let substr = &buf[start as usize..end as usize];
                if !canonical_positive_integer(substr) {
                    let start = self.u8_vec.len();
                    let end = start + substr.len();
                    self.u8_vec.extend_from_slice(substr);
                    let idx = self.atom_vec.len();
                    self.atom_vec.push(AtomBuf {
                        start: start as u32,
                        end: end as u32,
                    });
                    Ok(NodePtr::new(ObjectType::Bytes, idx))
                } else {
                    let mut new_val: u32 = 0;
                    for i in substr {
                        new_val <<= 8;
                        new_val |= *i as u32;
                    }
                    self.small_atoms += 1;
                    Ok(NodePtr::new(ObjectType::SmallAtom, new_val as usize))
                }
            }
        }
    }

    pub fn new_concat(&mut self, new_size: usize, nodes: &[NodePtr]) -> Result<NodePtr, EvalErr> {
        self.check_atom_limit()?;
        let start = self.u8_vec.len();
        if self.heap_limit - start < new_size {
            return err(self.nil(), "out of memory");
        }
        self.u8_vec.reserve(new_size);

        let mut counter: usize = 0;
        for node in nodes {
            match node.node_type() {
                (ObjectType::Pair, _) => {
                    self.u8_vec.truncate(start);
                    return err(*node, "(internal error) concat expected atom, got pair");
                }
                (ObjectType::Bytes, idx) => {
                    let term = self.atom_vec[idx as usize];
                    if counter + term.len() > new_size {
                        self.u8_vec.truncate(start);
                        return err(*node, "(internal error) concat passed invalid new_size");
                    }
                    self.u8_vec
                        .extend_from_within(term.start as usize..term.end as usize);
                    counter += term.len();
                }
                (ObjectType::SmallAtom, val) => {
                    let len = len_for_value(val) as u32;
                    let buf: [u8; 4] = val.to_be_bytes();
                    let buf = &buf[4 - len as usize..];
                    self.u8_vec.extend_from_slice(buf);
                    counter += len as usize;
                }
            }
        }
        if counter != new_size {
            self.u8_vec.truncate(start);
            return err(
                self.nil(),
                "(internal error) concat passed invalid new_size",
            );
        }
        let end = self.u8_vec.len() as u32;
        let idx = self.atom_vec.len();
        self.atom_vec.push(AtomBuf {
            start: (start as u32),
            end,
        });
        Ok(NodePtr::new(ObjectType::Bytes, idx))
    }

    pub fn atom_eq(&self, lhs: NodePtr, rhs: NodePtr) -> bool {
        match (lhs.node_type(), rhs.node_type()) {
            ((ObjectType::Pair, _), _) | (_, (ObjectType::Pair, _)) => {
                panic!("atom_eq() called on pair");
            }
            ((ObjectType::Bytes, lhs), (ObjectType::Bytes, rhs)) => {
                let lhs = self.atom_vec[lhs as usize];
                let rhs = self.atom_vec[rhs as usize];
                self.u8_vec[lhs.start as usize..lhs.end as usize]
                    == self.u8_vec[rhs.start as usize..rhs.end as usize]
            }
            ((ObjectType::SmallAtom, lhs), (ObjectType::SmallAtom, rhs)) => lhs == rhs,
            ((ObjectType::SmallAtom, val), (ObjectType::Bytes, idx))
            | ((ObjectType::Bytes, idx), (ObjectType::SmallAtom, val)) => {
                let atom = self.atom_vec[idx as usize];
                let len = len_for_value(val) as u32;
                if (atom.end - atom.start) != len {
                    return false;
                }
                if val == 0 {
                    return true;
                }

                if self.u8_vec[atom.start as usize] & 0x80 != 0 {
                    // SmallAtom only represents positive values
                    // if the byte buffer is negative, they can't match
                    return false;
                }

                // since we know the value of atom is small, we can turn it into a u32 and compare
                // against val
                let mut atom_val: u32 = 0;
                for i in atom.start..atom.end {
                    atom_val <<= 8;
                    atom_val |= self.u8_vec[i as usize] as u32;
                }
                val == atom_val
            }
        }
    }

    pub fn atom(&self, node: NodePtr) -> &[u8] {
        match node.node_type() {
            (ObjectType::Bytes, idx) => {
                let atom = self.atom_vec[idx as usize];
                &self.u8_vec[atom.start as usize..atom.end as usize]
            }
            (ObjectType::SmallAtom, val) => {
                let len = len_for_value(val);
                let mut idx = self.temp_idx.borrow_mut();
                *self.temp_vec[*idx].borrow_mut() = val.to_be_bytes();
                let ret = unsafe {
                    self.temp_vec[*idx]
                        .try_borrow_unguarded()
                        .expect("(internal error) temporary buffer problem in Allocator::atom()")
                };
                *idx += 1;
                if *idx == self.temp_vec.len() {
                    *idx = 0;
                }
                &ret[4 - len..]
            }
            _ => {
                panic!("expected atom, got pair");
            }
        }
    }

    pub fn atom_len(&self, node: NodePtr) -> usize {
        match node.node_type() {
            (ObjectType::Bytes, idx) => {
                let atom = self.atom_vec[idx as usize];
                (atom.end - atom.start) as usize
            }
            (ObjectType::SmallAtom, val) => len_for_value(val),
            _ => {
                panic!("expected atom, got pair");
            }
        }
    }

    pub fn small_number(&self, node: NodePtr) -> Option<u32> {
        match node.node_type() {
            (ObjectType::SmallAtom, val) => Some(val),
            _ => None,
        }
    }

    pub fn number(&self, node: NodePtr) -> Number {
        match node.node_type() {
            (ObjectType::Bytes, idx) => {
                let atom = self.atom_vec[idx as usize];
                number_from_u8(&self.u8_vec[atom.start as usize..atom.end as usize])
            }
            (ObjectType::SmallAtom, val) => Number::from(val),
            _ => {
                panic!("number() calld on pair");
            }
        }
    }

    pub fn g1(&self, node: NodePtr) -> Result<G1Element, EvalErr> {
        let idx = match node.node_type() {
            (ObjectType::Bytes, idx) => idx,
            (ObjectType::SmallAtom, _) => {
                return err(node, "atom is not G1 size, 48 bytes");
            }
            (ObjectType::Pair, _) => {
                return err(node, "pair found, expected G1 point");
            }
        };
        let atom = self.atom_vec[idx as usize];
        if atom.end - atom.start != 48 {
            return err(node, "atom is not G1 size, 48 bytes");
        }

        let array: &[u8; 48] = &self.u8_vec[atom.start as usize..atom.end as usize]
            .try_into()
            .expect("atom size is not 48 bytes");
        G1Element::from_bytes(array)
            .map_err(|_| EvalErr(node, "atom is not a G1 point".to_string()))
    }

    pub fn g2(&self, node: NodePtr) -> Result<G2Element, EvalErr> {
        let idx = match node.node_type() {
            (ObjectType::Bytes, idx) => idx,
            (ObjectType::SmallAtom, _) => {
                return err(node, "atom is not G2 size, 96 bytes");
            }
            (ObjectType::Pair, _) => {
                return err(node, "pair found, expected G2 point");
            }
        };
        let atom = self.atom_vec[idx as usize];
        if atom.end - atom.start != 96 {
            return err(node, "atom is not G2 size, 96 bytes");
        }

        let array: &[u8; 96] = &self.u8_vec[atom.start as usize..atom.end as usize]
            .try_into()
            .expect("atom size is not 96 bytes");

        G2Element::from_bytes(array)
            .map_err(|_| EvalErr(node, "atom is not a G2 point".to_string()))
    }

    pub fn node<'a>(&'a self, node: NodePtr) -> NodeVisitor<'a> {
        match node.node_type() {
            (ObjectType::Bytes, idx) => {
                let atom = self.atom_vec[idx as usize];
                let buf = &self.u8_vec[atom.start as usize..atom.end as usize];
                NodeVisitor::<'a>::Buffer(buf)
            }
            (ObjectType::SmallAtom, val) => NodeVisitor::U32(val),
            (ObjectType::Pair, idx) => {
                let pair = self.pair_vec[idx as usize];
                NodeVisitor::Pair(pair.first, pair.rest)
            }
        }
    }

    pub fn sexp(&self, node: NodePtr) -> SExp {
        match node.node_type() {
            (ObjectType::Bytes, _) | (ObjectType::SmallAtom, _) => SExp::Atom,
            (ObjectType::Pair, idx) => {
                let pair = self.pair_vec[idx as usize];
                SExp::Pair(pair.first, pair.rest)
            }
        }
    }

    // this is meant to be used when iterating lists:
    // while let Some((i, rest)) = a.next(node) {
    //     node = rest;
    //     ...
    // }
    pub fn next(&self, n: NodePtr) -> Option<(NodePtr, NodePtr)> {
        match self.sexp(n) {
            SExp::Pair(first, rest) => Some((first, rest)),
            SExp::Atom => None,
        }
    }

    pub fn nil(&self) -> NodePtr {
        NodePtr::new(ObjectType::SmallAtom, 0)
    }

    pub fn one(&self) -> NodePtr {
        NodePtr::new(ObjectType::SmallAtom, 1)
    }

    #[inline]
    fn check_atom_limit(&self) -> Result<(), EvalErr> {
        if self.atom_vec.len() + self.small_atoms == MAX_NUM_ATOMS {
            err(self.nil(), "too many atoms")
        } else {
            Ok(())
        }
    }

    #[cfg(feature = "counters")]
    pub fn atom_count(&self) -> usize {
        self.atom_vec.len()
    }

    #[cfg(feature = "counters")]
    pub fn small_atom_count(&self) -> usize {
        self.small_atoms
    }

    #[cfg(feature = "counters")]
    pub fn pair_count(&self) -> usize {
        self.pair_vec.len()
    }

    #[cfg(feature = "counters")]
    pub fn heap_size(&self) -> usize {
        self.u8_vec.len()
    }
}

#[test]
fn test_node_as_index() {
    assert_eq!(NodePtr::new(ObjectType::Pair, 0).as_index(), 0);
    assert_eq!(NodePtr::new(ObjectType::Pair, 1).as_index(), 3);
    assert_eq!(NodePtr::new(ObjectType::Pair, 2).as_index(), 6);
    assert_eq!(NodePtr::new(ObjectType::Pair, 3).as_index(), 9);
    assert_eq!(NodePtr::new(ObjectType::Bytes, 0).as_index(), 1);
    assert_eq!(NodePtr::new(ObjectType::Bytes, 1).as_index(), 4);
    assert_eq!(NodePtr::new(ObjectType::Bytes, 2).as_index(), 7);
    assert_eq!(NodePtr::new(ObjectType::Bytes, 3).as_index(), 10);
    assert_eq!(NodePtr::new(ObjectType::Bytes, 4).as_index(), 13);
}

#[test]
fn test_atom_eq_1() {
    // these are a bunch of different representations of 1
    // make sure they all compare equal
    let mut a = Allocator::new();
    let a0 = a.one();
    let a1 = a.new_atom(&[1]).unwrap();
    let a2 = {
        let tmp = a.new_atom(&[0x01, 0xff]).unwrap();
        a.new_substr(tmp, 0, 1).unwrap()
    };
    let a3 = a.new_substr(a2, 0, 1).unwrap();
    let a4 = a.new_number(1.into()).unwrap();
    let a5 = a.new_small_number(1).unwrap();

    assert!(a.atom_eq(a0, a0));
    assert!(a.atom_eq(a0, a1));
    assert!(a.atom_eq(a0, a2));
    assert!(a.atom_eq(a0, a3));
    assert!(a.atom_eq(a0, a4));
    assert!(a.atom_eq(a0, a5));

    assert!(a.atom_eq(a1, a0));
    assert!(a.atom_eq(a1, a1));
    assert!(a.atom_eq(a1, a2));
    assert!(a.atom_eq(a1, a3));
    assert!(a.atom_eq(a1, a4));
    assert!(a.atom_eq(a1, a5));

    assert!(a.atom_eq(a2, a0));
    assert!(a.atom_eq(a2, a1));
    assert!(a.atom_eq(a2, a2));
    assert!(a.atom_eq(a2, a3));
    assert!(a.atom_eq(a2, a4));
    assert!(a.atom_eq(a2, a5));

    assert!(a.atom_eq(a3, a0));
    assert!(a.atom_eq(a3, a1));
    assert!(a.atom_eq(a3, a2));
    assert!(a.atom_eq(a3, a3));
    assert!(a.atom_eq(a3, a4));
    assert!(a.atom_eq(a3, a5));

    assert!(a.atom_eq(a4, a0));
    assert!(a.atom_eq(a4, a1));
    assert!(a.atom_eq(a4, a2));
    assert!(a.atom_eq(a4, a3));
    assert!(a.atom_eq(a4, a4));
    assert!(a.atom_eq(a4, a5));

    assert!(a.atom_eq(a5, a0));
    assert!(a.atom_eq(a5, a1));
    assert!(a.atom_eq(a5, a2));
    assert!(a.atom_eq(a5, a3));
    assert!(a.atom_eq(a5, a4));
    assert!(a.atom_eq(a5, a5));
}

#[test]
fn test_atom_eq_minus_1() {
    // these are a bunch of different representations of -1
    // make sure they all compare equal
    let mut a = Allocator::new();
    let a0 = a.new_atom(&[0xff]).unwrap();
    let a1 = a.new_number((-1).into()).unwrap();
    let a2 = {
        let tmp = a.new_atom(&[0x01, 0xff]).unwrap();
        a.new_substr(tmp, 1, 2).unwrap()
    };
    let a3 = a.new_substr(a0, 0, 1).unwrap();

    assert!(a.atom_eq(a0, a0));
    assert!(a.atom_eq(a0, a1));
    assert!(a.atom_eq(a0, a2));
    assert!(a.atom_eq(a0, a3));

    assert!(a.atom_eq(a1, a0));
    assert!(a.atom_eq(a1, a1));
    assert!(a.atom_eq(a1, a2));
    assert!(a.atom_eq(a1, a3));

    assert!(a.atom_eq(a2, a0));
    assert!(a.atom_eq(a2, a1));
    assert!(a.atom_eq(a2, a2));
    assert!(a.atom_eq(a2, a3));

    assert!(a.atom_eq(a3, a0));
    assert!(a.atom_eq(a3, a1));
    assert!(a.atom_eq(a3, a2));
    assert!(a.atom_eq(a3, a3));
}

#[test]
fn test_atom_eq() {
    let mut a = Allocator::new();
    let a0 = a.nil();
    let a1 = a.one();
    let a2 = a.new_atom(&[1]).unwrap();
    let a3 = a.new_atom(&[0xfa, 0xc7]).unwrap();
    let a4 = a.new_small_number(1).unwrap();
    let a5 = a.new_number((-1337).into()).unwrap();

    assert!(a.atom_eq(a0, a0));
    assert!(!a.atom_eq(a0, a1));
    assert!(!a.atom_eq(a0, a2));
    assert!(!a.atom_eq(a0, a3));
    assert!(!a.atom_eq(a0, a4));
    assert!(!a.atom_eq(a0, a5));

    assert!(!a.atom_eq(a1, a0));
    assert!(a.atom_eq(a1, a1));
    assert!(a.atom_eq(a1, a2));
    assert!(!a.atom_eq(a1, a3));
    assert!(a.atom_eq(a1, a4));
    assert!(!a.atom_eq(a1, a5));

    assert!(!a.atom_eq(a2, a0));
    assert!(a.atom_eq(a2, a1));
    assert!(a.atom_eq(a2, a2));
    assert!(!a.atom_eq(a2, a3));
    assert!(a.atom_eq(a2, a4));
    assert!(!a.atom_eq(a2, a5));

    assert!(!a.atom_eq(a3, a0));
    assert!(!a.atom_eq(a3, a1));
    assert!(!a.atom_eq(a3, a2));
    assert!(a.atom_eq(a3, a3));
    assert!(!a.atom_eq(a3, a4));
    assert!(a.atom_eq(a3, a5));

    assert!(!a.atom_eq(a4, a0));
    assert!(a.atom_eq(a4, a1));
    assert!(a.atom_eq(a4, a2));
    assert!(!a.atom_eq(a4, a3));
    assert!(a.atom_eq(a4, a4));
    assert!(!a.atom_eq(a4, a5));
}

#[test]
#[should_panic]
fn test_atom_eq_pair1() {
    let mut a = Allocator::new();
    let a0 = a.nil();
    let pair = a.new_pair(a0, a0).unwrap();
    a.atom_eq(pair, a0);
}

#[test]
#[should_panic]
fn test_atom_eq_pair2() {
    let mut a = Allocator::new();
    let a0 = a.nil();
    let pair = a.new_pair(a0, a0).unwrap();
    a.atom_eq(a0, pair);
}

#[test]
#[should_panic]
fn test_atom_len_pair() {
    let mut a = Allocator::new();
    let a0 = a.nil();
    let pair = a.new_pair(a0, a0).unwrap();
    a.atom_len(pair);
}

#[test]
#[should_panic]
fn test_number_pair() {
    let mut a = Allocator::new();
    let a0 = a.nil();
    let pair = a.new_pair(a0, a0).unwrap();
    a.number(pair);
}

#[test]
#[should_panic]
fn test_invalid_node_ptr_type() {
    let node = NodePtr(3 << NODE_PTR_IDX_BITS);
    // unknown NodePtr type
    let _ = node.node_type();
}

#[cfg(dbg)]
#[test]
#[should_panic]
fn test_node_ptr_overflow() {
    NodePtr::new(ObjectType::Bytes, NODE_PTR_IDX_MASK + 1);
}

#[cfg(dbg)]
#[test]
#[should_panic]
fn test_invalid_small_number() {
    let mut a = Allocator::new();
    a.new_small_number(NODE_PTR_IDX_MASK + 1);
}

#[cfg(test)]
#[rstest]
#[case(0, 0)]
#[case(1, 1)]
#[case(0x7f, 1)]
#[case(0x80, 2)]
#[case(0x7fff, 2)]
#[case(0x7fffff, 3)]
#[case(0x800000, 4)]
#[case(0x7fffffff, 4)]
#[case(0x80000000, 5)]
#[case(0xffffffff, 5)]
fn test_len_for_value(#[case] val: u32, #[case] len: usize) {
    assert_eq!(len_for_value(val), len);
}

#[test]
fn test_nil() {
    let a = Allocator::new();
    assert_eq!(a.atom(a.nil()), b"");
    assert_eq!(a.sexp(a.nil()), SExp::Atom);
    assert_eq!(a.nil(), NodePtr::default());
    assert_eq!(a.nil(), NodePtr::NIL);
}

#[test]
fn test_one() {
    let a = Allocator::new();
    assert_eq!(a.atom(a.one()), b"\x01");
    assert_eq!(a.sexp(a.one()), SExp::Atom);
}

#[test]
fn test_allocate_atom() {
    let mut a = Allocator::new();
    let atom = a.new_atom(b"foobar").unwrap();
    assert_eq!(a.atom(atom), b"foobar");
    assert_eq!(a.sexp(atom), SExp::Atom);
}

#[test]
fn test_allocate_pair() {
    let mut a = Allocator::new();
    let atom1 = a.new_atom(b"foo").unwrap();
    let atom2 = a.new_atom(b"bar").unwrap();
    let pair = a.new_pair(atom1, atom2).unwrap();

    assert_eq!(a.sexp(pair), SExp::Pair(atom1, atom2));

    let pair2 = a.new_pair(pair, pair).unwrap();
    assert_eq!(a.sexp(pair2), SExp::Pair(pair, pair));
}

#[test]
fn test_allocate_heap_limit() {
    let mut a = Allocator::new_limited(6);
    // we can't allocate 6 bytes
    assert_eq!(a.new_atom(b"foobar").unwrap_err().1, "out of memory");
    // but 5 is OK
    let _atom = a.new_atom(b"fooba").unwrap();
}

#[test]
fn test_allocate_atom_limit() {
    let mut a = Allocator::new();

    for _ in 0..MAX_NUM_ATOMS - 2 {
        // exhaust the number of atoms allowed to be allocated
        let _ = a.new_atom(b"foo").unwrap();
    }
    assert_eq!(a.new_atom(b"foobar").unwrap_err().1, "too many atoms");
    assert_eq!(a.u8_vec.len(), 0);
    assert_eq!(a.small_atoms, MAX_NUM_ATOMS);
}

#[test]
fn test_allocate_small_number_limit() {
    let mut a = Allocator::new();

    for _ in 0..MAX_NUM_ATOMS - 2 {
        // exhaust the number of atoms allowed to be allocated
        let _ = a.new_atom(b"foo").unwrap();
    }
    assert_eq!(a.new_small_number(3).unwrap_err().1, "too many atoms");
    assert_eq!(a.u8_vec.len(), 0);
    assert_eq!(a.small_atoms, MAX_NUM_ATOMS);
}

#[test]
fn test_allocate_substr_limit() {
    let mut a = Allocator::new();

    for _ in 0..MAX_NUM_ATOMS - 3 {
        // exhaust the number of atoms allowed to be allocated
        let _ = a.new_atom(b"foo").unwrap();
    }
    let atom = a.new_atom(b"foo").unwrap();
    assert_eq!(a.new_substr(atom, 1, 2).unwrap_err().1, "too many atoms");
    assert_eq!(a.u8_vec.len(), 0);
    assert_eq!(a.small_atoms, MAX_NUM_ATOMS);
}

#[test]
fn test_allocate_concat_limit() {
    let mut a = Allocator::new();

    for _ in 0..MAX_NUM_ATOMS - 3 {
        // exhaust the number of atoms allowed to be allocated
        let _ = a.new_atom(b"foo").unwrap();
    }
    let atom = a.new_atom(b"foo").unwrap();
    assert_eq!(a.new_concat(3, &[atom]).unwrap_err().1, "too many atoms");
    assert_eq!(a.u8_vec.len(), 0);
    assert_eq!(a.small_atoms, MAX_NUM_ATOMS);
}

#[test]
fn test_allocate_pair_limit() {
    let mut a = Allocator::new();
    let atom = a.new_atom(b"foo").unwrap();
    // one pair is OK
    let _pair1 = a.new_pair(atom, atom).unwrap();
    for _ in 1..MAX_NUM_PAIRS {
        // exhaust the number of pairs allowed to be allocated
        let _ = a.new_pair(atom, atom).unwrap();
    }

    assert_eq!(a.new_pair(atom, atom).unwrap_err().1, "too many pairs");
}

#[test]
fn test_substr() {
    let mut a = Allocator::new();
    let atom = a.new_atom(b"foobar").unwrap();
    let pair = a.new_pair(atom, atom).unwrap();

    let sub = a.new_substr(atom, 0, 1).unwrap();
    assert_eq!(a.atom(sub), b"f");
    let sub = a.new_substr(atom, 1, 6).unwrap();
    assert_eq!(a.atom(sub), b"oobar");
    let sub = a.new_substr(atom, 1, 1).unwrap();
    assert_eq!(a.atom(sub), b"");
    let sub = a.new_substr(atom, 0, 0).unwrap();
    assert_eq!(a.atom(sub), b"");

    assert_eq!(
        a.new_substr(atom, 1, 0).unwrap_err().1,
        "substr invalid bounds"
    );
    assert_eq!(
        a.new_substr(atom, 7, 7).unwrap_err().1,
        "substr start out of bounds"
    );
    assert_eq!(
        a.new_substr(atom, 0, 7).unwrap_err().1,
        "substr end out of bounds"
    );
    assert_eq!(
        a.new_substr(atom, u32::MAX, 4).unwrap_err().1,
        "substr start out of bounds"
    );
    assert_eq!(
        a.new_substr(pair, 0, 0).unwrap_err().1,
        "(internal error) substr expected atom, got pair"
    );
}

#[test]
fn test_substr_small_number() {
    let mut a = Allocator::new();
    let atom = a.new_atom(b"a\x80").unwrap();
    assert!(a.small_number(atom).is_some());

    let sub = a.new_substr(atom, 0, 1).unwrap();
    assert_eq!(a.atom(sub), b"a");
    assert!(a.small_number(sub).is_some());
    let sub = a.new_substr(atom, 1, 2).unwrap();
    assert_eq!(a.atom(sub), b"\x80");
    assert!(a.small_number(sub).is_none());
    let sub = a.new_substr(atom, 1, 1).unwrap();
    assert_eq!(a.atom(sub), b"");
    let sub = a.new_substr(atom, 0, 0).unwrap();
    assert_eq!(a.atom(sub), b"");

    assert_eq!(
        a.new_substr(atom, 1, 0).unwrap_err().1,
        "substr invalid bounds"
    );
    assert_eq!(
        a.new_substr(atom, 3, 3).unwrap_err().1,
        "substr start out of bounds"
    );
    assert_eq!(
        a.new_substr(atom, 0, 3).unwrap_err().1,
        "substr end out of bounds"
    );
    assert_eq!(
        a.new_substr(atom, u32::MAX, 2).unwrap_err().1,
        "substr start out of bounds"
    );
}

#[test]
fn test_concat() {
    let mut a = Allocator::new();
    let atom1 = a.new_atom(b"f").unwrap();
    let atom2 = a.new_atom(b"o").unwrap();
    let atom3 = a.new_atom(b"o").unwrap();
    let atom4 = a.new_atom(b"b").unwrap();
    let atom5 = a.new_atom(b"a").unwrap();
    let atom6 = a.new_atom(b"r").unwrap();
    let pair = a.new_pair(atom1, atom2).unwrap();

    let cat = a
        .new_concat(6, &[atom1, atom2, atom3, atom4, atom5, atom6])
        .unwrap();
    assert_eq!(a.atom(cat), b"foobar");

    let cat = a.new_concat(12, &[cat, cat]).unwrap();
    assert_eq!(a.atom(cat), b"foobarfoobar");

    assert_eq!(
        a.new_concat(11, &[cat, cat]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
    assert_eq!(
        a.new_concat(13, &[cat, cat]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
    assert_eq!(
        a.new_concat(12, &[atom3, pair]).unwrap_err().1,
        "(internal error) concat expected atom, got pair"
    );

    assert_eq!(
        a.new_concat(4, &[atom1, atom2, atom3]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );

    assert_eq!(
        a.new_concat(2, &[atom1, atom2, atom3]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
}

#[test]
fn test_concat_large() {
    let mut a = Allocator::new();
    let atom1 = a.new_atom(b"foo").unwrap();
    let atom2 = a.new_atom(b"bar").unwrap();
    let pair = a.new_pair(atom1, atom2).unwrap();

    let cat = a.new_concat(6, &[atom1, atom2]).unwrap();
    assert_eq!(a.atom(cat), b"foobar");

    let cat = a.new_concat(12, &[cat, cat]).unwrap();
    assert_eq!(a.atom(cat), b"foobarfoobar");

    assert_eq!(
        a.new_concat(11, &[cat, cat]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
    assert_eq!(
        a.new_concat(13, &[cat, cat]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
    assert_eq!(
        a.new_concat(12, &[atom1, pair]).unwrap_err().1,
        "(internal error) concat expected atom, got pair"
    );

    assert_eq!(
        a.new_concat(4, &[atom1, atom2]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );

    assert_eq!(
        a.new_concat(2, &[atom1, atom2]).unwrap_err().1,
        "(internal error) concat passed invalid new_size"
    );
}

#[test]
fn test_sexp() {
    let mut a = Allocator::new();
    let atom1 = a.new_atom(b"f").unwrap();
    let atom2 = a.new_atom(b"o").unwrap();
    let pair = a.new_pair(atom1, atom2).unwrap();

    assert_eq!(a.sexp(atom1), SExp::Atom);
    assert_eq!(a.sexp(atom2), SExp::Atom);
    assert_eq!(a.sexp(pair), SExp::Pair(atom1, atom2));
}

#[test]
fn test_concat_limit() {
    let mut a = Allocator::new_limited(6);
    let atom1 = a.new_atom(b"f").unwrap();
    let atom2 = a.new_atom(b"o").unwrap();
    let atom3 = a.new_atom(b"o").unwrap();
    let atom4 = a.new_atom(b"b").unwrap();
    let atom5 = a.new_atom(b"a").unwrap();
    let atom6 = a.new_atom(b"r").unwrap();

    // we only have 2 bytes left of allowed heap allocation
    assert_eq!(
        a.new_concat(6, &[atom1, atom2, atom3, atom4, atom5, atom6])
            .unwrap_err()
            .1,
        "out of memory"
    );
    let cat = a.new_concat(2, &[atom1, atom2]).unwrap();
    assert_eq!(a.atom(cat), b"fo");
}

#[cfg(test)]
use rstest::rstest;

#[cfg(test)]
#[rstest]
#[case(0.into(), &[])]
#[case(1.into(), &[1])]
#[case((-1).into(), &[0xff])]
#[case(0x80.into(), &[0, 0x80])]
#[case(0xff.into(), &[0, 0xff])]
#[case(0xffffffff_u64.into(), &[0, 0xff, 0xff, 0xff, 0xff])]
fn test_new_number(#[case] num: Number, #[case] expected: &[u8]) {
    let mut a = Allocator::new();

    // TEST creating the atom from a Number
    let atom = a.new_number(num.clone()).unwrap();

    // make sure we get back the same number
    assert_eq!(a.number(atom), num);
    assert_eq!(a.atom(atom), expected);
    assert_eq!(number_from_u8(expected), num);

    // TEST creating the atom from a buffer
    let atom = a.new_atom(expected).unwrap();

    // make sure we get back the same number
    assert_eq!(a.number(atom), num);
    assert_eq!(a.atom(atom), expected);
    assert_eq!(number_from_u8(expected), num);
}

#[test]
fn test_checkpoints() {
    let mut a = Allocator::new();

    let atom1 = a.new_atom(&[4, 3, 2, 1]).unwrap();
    assert!(a.atom(atom1) == [4, 3, 2, 1]);

    let checkpoint = a.checkpoint();

    let atom2 = a.new_atom(&[6, 5, 4, 3]).unwrap();
    assert!(a.atom(atom1) == [4, 3, 2, 1]);
    assert!(a.atom(atom2) == [6, 5, 4, 3]);

    // at this point we have two atoms and a checkpoint from before the second
    // atom was created

    // now, restoring the checkpoint state will make atom2 disappear

    a.restore_checkpoint(&checkpoint);

    assert!(a.atom(atom1) == [4, 3, 2, 1]);
    let atom3 = a.new_atom(&[6, 5, 4, 3]).unwrap();
    assert!(a.atom(atom3) == [6, 5, 4, 3]);

    // since atom2 was removed, atom3 should actually be using that slot
    assert_eq!(atom2, atom3);
}

#[cfg(test)]
fn test_g1(a: &Allocator, n: NodePtr) -> EvalErr {
    a.g1(n).unwrap_err()
}

#[cfg(test)]
fn test_g2(a: &Allocator, n: NodePtr) -> EvalErr {
    a.g2(n).unwrap_err()
}

#[cfg(test)]
type TestFun = fn(&Allocator, NodePtr) -> EvalErr;

#[cfg(test)]
#[rstest]
#[case(test_g1, 0, "atom is not G1 size, 48 bytes")]
#[case(test_g1, 3, "atom is not G1 size, 48 bytes")]
#[case(test_g1, 47, "atom is not G1 size, 48 bytes")]
#[case(test_g1, 49, "atom is not G1 size, 48 bytes")]
#[case(test_g1, 48, "atom is not a G1 point")]
#[case(test_g2, 0, "atom is not G2 size, 96 bytes")]
#[case(test_g2, 3, "atom is not G2 size, 96 bytes")]
#[case(test_g2, 95, "atom is not G2 size, 96 bytes")]
#[case(test_g2, 97, "atom is not G2 size, 96 bytes")]
#[case(test_g2, 96, "atom is not a G2 point")]
fn test_point_size_error(#[case] fun: TestFun, #[case] size: usize, #[case] expected: &str) {
    let mut a = Allocator::new();
    let mut buf = Vec::<u8>::new();
    buf.resize(size, 0xcc);
    let n = a.new_atom(&buf).unwrap();
    let r = fun(&a, n);
    assert_eq!(r.0, n);
    assert_eq!(r.1, expected.to_string());
}

#[cfg(test)]
#[rstest]
#[case(test_g1, "pair found, expected G1 point")]
#[case(test_g2, "pair found, expected G2 point")]
fn test_point_atom_pair(#[case] fun: TestFun, #[case] expected: &str) {
    let mut a = Allocator::new();
    let n = a.new_pair(a.nil(), a.one()).unwrap();
    let r = fun(&a, n);
    assert_eq!(r.0, n);
    assert_eq!(r.1, expected.to_string());
}

#[cfg(test)]
#[rstest]
#[case(
    "\
97f1d3a73197d7942695638c4fa9ac0f\
c3688c4f9774b905a14e3a3f171bac58\
6c55e83ff97a1aeffb3af00adb22c6bb"
)]
#[case(
    "\
a572cbea904d67468808c8eb50a9450c\
9721db309128012543902d0ac358a62a\
e28f75bb8f1c7c42c39a8c5529bf0f4e"
)]
fn test_g1_roundtrip(#[case] atom: &str) {
    let mut a = Allocator::new();
    let n = a.new_atom(&hex::decode(atom).unwrap()).unwrap();
    let g1 = a.g1(n).unwrap();
    assert_eq!(hex::encode(g1.to_bytes()), atom);

    let g1_copy = a.new_g1(g1).unwrap();
    let g1_atom = a.atom(g1_copy);
    assert_eq!(hex::encode(g1_atom), atom);

    // try interpreting the point as G1
    assert_eq!(a.g2(n).unwrap_err().1, "atom is not G2 size, 96 bytes");
    assert_eq!(
        a.g2(g1_copy).unwrap_err().1,
        "atom is not G2 size, 96 bytes"
    );

    // try interpreting the point as number
    assert_eq!(a.number(n), number_from_u8(&hex::decode(atom).unwrap()));
    assert_eq!(
        a.number(g1_copy),
        number_from_u8(&hex::decode(atom).unwrap())
    );
}

#[cfg(test)]
#[rstest]
#[case(
    "\
93e02b6052719f607dacd3a088274f65\
596bd0d09920b61ab5da61bbdc7f5049\
334cf11213945d57e5ac7d055d042b7e\
024aa2b2f08f0a91260805272dc51051\
c6e47ad4fa403b02b4510b647ae3d177\
0bac0326a805bbefd48056c8c121bdb8"
)]
#[case(
    "\
aa4edef9c1ed7f729f520e47730a124f\
d70662a904ba1074728114d1031e1572\
c6c886f6b57ec72a6178288c47c33577\
1638533957d540a9d2370f17cc7ed586\
3bc0b995b8825e0ee1ea1e1e4d00dbae\
81f14b0bf3611b78c952aacab827a053"
)]
fn test_g2_roundtrip(#[case] atom: &str) {
    let mut a = Allocator::new();
    let n = a.new_atom(&hex::decode(atom).unwrap()).unwrap();
    let g2 = a.g2(n).unwrap();
    assert_eq!(hex::encode(g2.to_bytes()), atom);

    let g2_copy = a.new_g2(g2).unwrap();
    let g2_atom = a.atom(g2_copy);
    assert_eq!(hex::encode(g2_atom), atom);

    // try interpreting the point as G1
    assert_eq!(a.g1(n).unwrap_err().1, "atom is not G1 size, 48 bytes");
    assert_eq!(
        a.g1(g2_copy).unwrap_err().1,
        "atom is not G1 size, 48 bytes"
    );

    // try interpreting the point as number
    assert_eq!(a.number(n), number_from_u8(&hex::decode(atom).unwrap()));
    assert_eq!(
        a.number(g2_copy),
        number_from_u8(&hex::decode(atom).unwrap())
    );
}

#[cfg(test)]
use core::convert::TryFrom;

#[cfg(test)]
type MakeFun = fn(&mut Allocator, &[u8]) -> NodePtr;

#[cfg(test)]
fn make_buf(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    a.new_atom(bytes).unwrap()
}

#[cfg(test)]
fn make_number(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    let v = number_from_u8(bytes);
    a.new_number(v).unwrap()
}

#[cfg(test)]
fn make_g1(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    let v = G1Element::from_bytes(bytes.try_into().unwrap()).unwrap();
    a.new_g1(v).unwrap()
}

#[cfg(test)]
fn make_g2(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    let v = G2Element::from_bytes(bytes.try_into().unwrap()).unwrap();
    a.new_g2(v).unwrap()
}

#[cfg(test)]
fn make_g1_fail(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    assert!(<[u8; 48]>::try_from(bytes).is_err());
    a.new_atom(bytes).unwrap()
}

#[cfg(test)]
fn make_g2_fail(a: &mut Allocator, bytes: &[u8]) -> NodePtr {
    assert!(<[u8; 96]>::try_from(bytes).is_err());
    a.new_atom(bytes).unwrap()
}

#[cfg(test)]
type CheckFun = fn(&Allocator, NodePtr, &[u8]);

#[cfg(test)]
fn check_buf(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    let buf = a.atom(n);
    assert_eq!(buf, bytes);
}

#[cfg(test)]
fn check_number(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    let num = a.number(n);
    let v = number_from_u8(bytes);
    assert_eq!(num, v);
}

#[cfg(test)]
fn check_g1(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    let num = a.g1(n).unwrap();
    let v = G1Element::from_bytes(bytes.try_into().unwrap()).unwrap();
    assert_eq!(num, v);
}

#[cfg(test)]
fn check_g2(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    let num = a.g2(n).unwrap();
    let v = G2Element::from_bytes(bytes.try_into().unwrap()).unwrap();
    assert_eq!(num, v);
}

#[cfg(test)]
fn check_g1_fail(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    assert_eq!(a.g1(n).unwrap_err().0, n);
    assert!(<[u8; 48]>::try_from(bytes).is_err());
}

#[cfg(test)]
fn check_g2_fail(a: &Allocator, n: NodePtr, bytes: &[u8]) {
    assert_eq!(a.g2(n).unwrap_err().0, n);
    assert!(<[u8; 96]>::try_from(bytes).is_err());
}

#[cfg(test)]
const EMPTY: &str = "";

#[cfg(test)]
const SMALL_BUF: &str = "133742";

#[cfg(test)]
const VALID_G1: &str = "\
a572cbea904d67468808c8eb50a9450c\
9721db309128012543902d0ac358a62a\
e28f75bb8f1c7c42c39a8c5529bf0f4e";

#[cfg(test)]
const VALID_G2: &str = "\
aa4edef9c1ed7f729f520e47730a124f\
d70662a904ba1074728114d1031e1572\
c6c886f6b57ec72a6178288c47c33577\
1638533957d540a9d2370f17cc7ed586\
3bc0b995b8825e0ee1ea1e1e4d00dbae\
81f14b0bf3611b78c952aacab827a053";

/*
  We want to exercise round-tripping avery kind of value via every other kind
  of value (as far as possible). e.g. Every value can round-trip through a byte buffer
  or a number, but G1 cannot round-trip via G2.

  +-----------+--------+--------+------+------+
  | from / to | buffer | number | G1   | G2   |
  +-----------+--------+--------+------+------+
  | buffer    | o      | o      | -    | -    |
  | number    | o      | o      | -    | -    |
  | G1        | o      | o      | o    | -    |
  | G2        | o      | o      | -    | o    |
  +-----------+--------+--------+------+------+

*/

#[cfg(test)]
#[rstest]
// round trip empty buffer
#[case(EMPTY, make_buf, check_buf)]
#[case(EMPTY, make_buf, check_number)]
#[case(EMPTY, make_buf, check_g1_fail)]
#[case(EMPTY, make_buf, check_g2_fail)]
#[case(EMPTY, make_number, check_buf)]
#[case(EMPTY, make_number, check_number)]
#[case(EMPTY, make_number, check_g1_fail)]
#[case(EMPTY, make_number, check_g2_fail)]
#[case(EMPTY, make_g1_fail, check_buf)]
#[case(EMPTY, make_g1_fail, check_number)]
#[case(EMPTY, make_g1_fail, check_g1_fail)]
#[case(EMPTY, make_g1_fail, check_g2_fail)]
#[case(EMPTY, make_g2_fail, check_buf)]
#[case(EMPTY, make_g2_fail, check_number)]
#[case(EMPTY, make_g2_fail, check_g1_fail)]
#[case(EMPTY, make_g2_fail, check_g2_fail)]
// round trip small buffer
#[case(SMALL_BUF, make_buf, check_buf)]
#[case(SMALL_BUF, make_buf, check_number)]
#[case(SMALL_BUF, make_buf, check_g1_fail)]
#[case(SMALL_BUF, make_buf, check_g2_fail)]
#[case(SMALL_BUF, make_number, check_buf)]
#[case(SMALL_BUF, make_number, check_number)]
#[case(SMALL_BUF, make_number, check_g1_fail)]
#[case(SMALL_BUF, make_number, check_g2_fail)]
#[case(SMALL_BUF, make_g1_fail, check_buf)]
#[case(SMALL_BUF, make_g1_fail, check_number)]
#[case(SMALL_BUF, make_g1_fail, check_g1_fail)]
#[case(SMALL_BUF, make_g1_fail, check_g2_fail)]
#[case(SMALL_BUF, make_g2_fail, check_buf)]
#[case(SMALL_BUF, make_g2_fail, check_number)]
#[case(SMALL_BUF, make_g2_fail, check_g1_fail)]
#[case(SMALL_BUF, make_g2_fail, check_g2_fail)]
// round trip G1 point
#[case(VALID_G1, make_buf, check_buf)]
#[case(VALID_G1, make_buf, check_number)]
#[case(VALID_G1, make_buf, check_g1)]
#[case(VALID_G1, make_buf, check_g2_fail)]
#[case(VALID_G1, make_number, check_buf)]
#[case(VALID_G1, make_number, check_number)]
#[case(VALID_G1, make_number, check_g1)]
#[case(VALID_G1, make_number, check_g2_fail)]
#[case(VALID_G1, make_g1, check_buf)]
#[case(VALID_G1, make_g1, check_number)]
#[case(VALID_G1, make_g1, check_g1)]
#[case(VALID_G1, make_g1, check_g2_fail)]
#[case(VALID_G1, make_g2_fail, check_buf)]
#[case(VALID_G1, make_g2_fail, check_number)]
#[case(VALID_G1, make_g2_fail, check_g1)]
#[case(VALID_G1, make_g2_fail, check_g2_fail)]
// round trip G2 point
#[case(VALID_G2, make_buf, check_buf)]
#[case(VALID_G2, make_buf, check_number)]
#[case(VALID_G2, make_buf, check_g1_fail)]
#[case(VALID_G2, make_buf, check_g2)]
#[case(VALID_G2, make_number, check_buf)]
#[case(VALID_G2, make_number, check_number)]
#[case(VALID_G2, make_number, check_g1_fail)]
#[case(VALID_G2, make_number, check_g2)]
#[case(VALID_G2, make_g1_fail, check_buf)]
#[case(VALID_G2, make_g1_fail, check_number)]
#[case(VALID_G2, make_g1_fail, check_g1_fail)]
#[case(VALID_G2, make_g1_fail, check_g2)]
#[case(VALID_G2, make_g2, check_buf)]
#[case(VALID_G2, make_g2, check_number)]
#[case(VALID_G2, make_g2, check_g1_fail)]
#[case(VALID_G2, make_g2, check_g2)]
fn test_roundtrip(#[case] test_value: &str, #[case] make: MakeFun, #[case] check: CheckFun) {
    let value = hex::decode(test_value).unwrap();
    let mut a = Allocator::new();
    let node = make(&mut a, &value);
    check(&a, node, &value);
}

#[cfg(test)]
#[rstest]
#[case(&[], 0)]
#[case(&[1], 1)]
#[case(&[1,2], 2)]
#[case(&[1,2,3,4,5,6,7,8,9], 9)]
#[case(&[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18], 18)]
fn test_atom_len(#[case] buf: &[u8], #[case] expected: usize) {
    let mut a = Allocator::new();
    let atom = a.new_atom(buf).unwrap();
    assert_eq!(a.atom_len(atom), expected);
}

#[cfg(test)]
#[rstest]
#[case(0.into(), 0)]
#[case(42.into(), 1)]
#[case(127.into(), 1)]
#[case(1337.into(), 2)]
#[case(0x7fffff.into(), 3)]
#[case(0xffffff.into(), 4)]
#[case((-1).into(), 1)]
#[case((-128).into(), 1)]
fn test_atom_len_number(#[case] value: Number, #[case] expected: usize) {
    let mut a = Allocator::new();
    let atom = a.new_number(value).unwrap();
    assert_eq!(a.atom_len(atom), expected);
}

#[cfg(test)]
#[rstest]
#[case(
    "\
97f1d3a73197d7942695638c4fa9ac0f\
c3688c4f9774b905a14e3a3f171bac58\
6c55e83ff97a1aeffb3af00adb22c6bb",
    48
)]
#[case(
    "\
a572cbea904d67468808c8eb50a9450c\
9721db309128012543902d0ac358a62a\
e28f75bb8f1c7c42c39a8c5529bf0f4e",
    48
)]
fn test_atom_len_g1(#[case] buffer_hex: &str, #[case] expected: usize) {
    let mut a = Allocator::new();
    let buffer = &hex::decode(buffer_hex).unwrap();
    let g1 = G1Element::from_bytes(&buffer[..].try_into().unwrap()).expect("invalid G1 point");
    let atom = a.new_g1(g1).unwrap();
    assert_eq!(a.atom_len(atom), expected);
}

#[cfg(test)]
#[rstest]
#[case(
    "\
93e02b6052719f607dacd3a088274f65\
596bd0d09920b61ab5da61bbdc7f5049\
334cf11213945d57e5ac7d055d042b7e\
024aa2b2f08f0a91260805272dc51051\
c6e47ad4fa403b02b4510b647ae3d177\
0bac0326a805bbefd48056c8c121bdb8",
    96
)]
#[case(
    "\
aa4edef9c1ed7f729f520e47730a124f\
d70662a904ba1074728114d1031e1572\
c6c886f6b57ec72a6178288c47c33577\
1638533957d540a9d2370f17cc7ed586\
3bc0b995b8825e0ee1ea1e1e4d00dbae\
81f14b0bf3611b78c952aacab827a053",
    96
)]
fn test_atom_len_g2(#[case] buffer_hex: &str, #[case] expected: usize) {
    let mut a = Allocator::new();

    let buffer = &hex::decode(buffer_hex).unwrap();
    let g2 = G2Element::from_bytes(&buffer[..].try_into().unwrap()).expect("invalid G2 point");
    let atom = a.new_g2(g2).unwrap();
    assert_eq!(a.atom_len(atom), expected);
}

#[cfg(test)]
#[rstest]
#[case(0.into())]
#[case(1.into())]
#[case(0x7f.into())]
#[case(0x80.into())]
#[case(0xff.into())]
#[case(0x100.into())]
#[case(0x7fff.into())]
#[case(0x8000.into())]
#[case(0xffff.into())]
#[case(0x10000.into())]
#[case(0x7ffff.into())]
#[case(0x80000.into())]
#[case(0xfffff.into())]
#[case(0x100000.into())]
#[case(0x7ffffff.into())]
#[case(0x8000000.into())]
#[case(0xfffffff.into())]
#[case(0x10000000.into())]
#[case(0x7ffffffff_u64.into())]
#[case(0x8000000000_u64.into())]
#[case(0xffffffffff_u64.into())]
#[case(0x10000000000_u64.into())]
#[case((-1).into())]
#[case((-0x7f).into())]
#[case((-0x80).into())]
#[case((-0xff).into())]
#[case((-0x100).into())]
#[case((-0x7fff).into())]
#[case((-0x8000).into())]
#[case((-0xffff).into())]
#[case((-0x10000).into())]
#[case((-0x7ffff).into())]
#[case((-0x80000).into())]
#[case((-0xfffff).into())]
#[case((-0x100000).into())]
#[case((-0x7ffffff_i64).into())]
#[case((-0x8000000_i64).into())]
#[case((-0xfffffff_i64).into())]
#[case((-0x10000000_i64).into())]
#[case((-0x7ffffffff_i64).into())]
#[case((-0x8000000000_i64).into())]
#[case((-0xffffffffff_i64).into())]
#[case((-0x10000000000_i64).into())]
fn test_number_roundtrip(#[case] value: Number) {
    let mut a = Allocator::new();
    let atom = a.new_number(value.clone()).expect("new_number()");
    assert_eq!(a.number(atom), value);
}

#[cfg(test)]
#[rstest]
#[case(0)]
#[case(1)]
#[case(0x7f)]
#[case(0x80)]
#[case(0xff)]
#[case(0x100)]
#[case(0x7fff)]
#[case(0x8000)]
#[case(0xffff)]
#[case(0x10000)]
#[case(0x7ffff)]
#[case(0x80000)]
#[case(0xfffff)]
#[case(0x100000)]
#[case(0x7fffff)]
#[case(0x800000)]
#[case(0xffffff)]
#[case(0x1000000)]
#[case(0x3ffffff)]
fn test_small_number_roundtrip(#[case] value: u32) {
    let mut a = Allocator::new();
    let atom = a.new_small_number(value).expect("new_small_number()");
    assert_eq!(a.small_number(atom).expect("small_number()"), value);
}

#[cfg(test)]
#[rstest]
#[case(0.into(), true)]
#[case(1.into(), true)]
#[case(0x3ffffff.into(), true)]
#[case(0x4000000.into(), false)]
#[case(0x7f.into(), true)]
#[case(0x80.into(), true)]
#[case(0xff.into(), true)]
#[case(0x100.into(), true)]
#[case(0x7fff.into(), true)]
#[case(0x8000.into(), true)]
#[case(0xffff.into(), true)]
#[case(0x10000.into(), true)]
#[case(0x7ffff.into(), true)]
#[case(0x80000.into(), true)]
#[case(0xfffff.into(), true)]
#[case(0x100000.into(), true)]
#[case(0x7ffffff.into(), false)]
#[case(0x8000000.into(), false)]
#[case(0xfffffff.into(), false)]
#[case(0x10000000.into(), false)]
#[case(0x7ffffffff_u64.into(), false)]
#[case(0x8000000000_u64.into(), false )]
#[case(0xffffffffff_u64.into(), false)]
#[case(0x10000000000_u64.into(), false)]
#[case((-1).into(), false)]
#[case((-0x7f).into(), false)]
#[case((-0x80).into(), false)]
#[case((-0x10000000000_i64).into(), false)]
fn test_auto_small_number(#[case] value: Number, #[case] expect_small: bool) {
    let mut a = Allocator::new();
    let atom = a.new_number(value.clone()).expect("new_number()");
    assert_eq!(a.small_number(atom).is_some(), expect_small);
    if let Some(v) = a.small_number(atom) {
        use num_traits::ToPrimitive;
        assert_eq!(v, value.to_u32().unwrap());
    }
    assert_eq!(a.number(atom), value);
}

#[cfg(test)]
#[rstest]
// redundant leading zeros are not canoncial
#[case(&[0x00], false)]
#[case(&[0x00, 0x7f], false)]
// negative numbers cannot be small ints
#[case(&[0x80], false)]
#[case(&[0xff], false)]
#[case(&[0xff, 0xff], false)]
#[case(&[0x80, 0xff, 0xff], false)]
// we use a simple heuristic, for atoms. if we have more than 3 bytes, we assume
// it's not small. Even though it would have fit in 26 bits
#[case(&[0x1, 0xff, 0xff, 0xff], false)]
// small positive intergers can be small
#[case(&[0x01], true)]
#[case(&[0x00, 0xff], true)]
#[case(&[0x7f, 0xff], true)]
#[case(&[0x7f, 0xff, 0xff], true)]
fn test_auto_small_number_from_buf(#[case] buf: &[u8], #[case] expect_small: bool) {
    let mut a = Allocator::new();
    let atom = a.new_atom(buf).expect("new_atom()");
    assert_eq!(a.small_number(atom).is_some(), expect_small);
    if let Some(v) = a.small_number(atom) {
        use num_traits::ToPrimitive;
        assert_eq!(v, a.number(atom).to_u32().expect("to_u32()"));
    }
    assert_eq!(buf, a.atom(atom));
}
