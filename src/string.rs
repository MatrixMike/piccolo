use std::{
    alloc,
    borrow::Cow,
    fmt,
    hash::{BuildHasherDefault, Hash, Hasher},
    io::Write,
    ops, slice,
    str::{self, Utf8Error},
    string::String as StdString,
};

use gc_arena::{
    allocator_api::MetricsAlloc, barrier::Unlock, lock::RefLock, metrics::Metrics, Collect, Gc,
    GcWeak, Mutation, StaticCollect,
};
use hashbrown::{hash_map, raw::RawTable, HashMap};
use rustc_hash::FxHasher;
use thiserror::Error;

use crate::{Context, Value};

// Represents `String` as either a pointer to an external / owned slice pointer or a size prefixed
// inline array.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct String<'gc>(Gc<'gc, Header>);

#[derive(Copy, Clone, Collect)]
#[collect(require_static)]
struct Header {
    hash: u64,
    buffer: Buffer,
}

#[derive(Copy, Clone)]
enum Buffer {
    Indirect(*const [u8]),
    Inline(usize),
}

impl<'gc> String<'gc> {
    pub fn from_buffer(mc: &Mutation<'gc>, s: Box<[u8]>) -> String<'gc> {
        #[derive(Collect)]
        #[collect(require_static)]
        #[repr(C)]
        struct Owned {
            header: Header,
            metrics: Metrics,
        }

        impl Drop for Owned {
            fn drop(&mut self) {
                match self.header.buffer {
                    Buffer::Indirect(ptr) => unsafe {
                        self.metrics.mark_external_deallocation((*ptr).len());
                        drop(Box::from_raw(ptr as *mut [u8]));
                    },
                    Buffer::Inline(_) => unreachable!(),
                }
            }
        }

        let metrics = mc.metrics().clone();
        metrics.mark_external_allocation(s.len());
        let owned = Owned {
            header: Header {
                hash: str_hash(&s),
                buffer: Buffer::Indirect(Box::into_raw(s)),
            },
            metrics,
        };
        // SAFETY: We know we can cast to `InlineHeader` because `Owned` is `#[repr(C)]`
        String(unsafe { Gc::cast::<Header>(Gc::new(mc, owned)) })
    }

    pub fn from_slice(mc: &Mutation<'gc>, s: impl AsRef<[u8]>) -> String<'gc> {
        // TODO: This is an extremely silly way to allocate a dynamically sized, inline string.
        // Since gc-arena does not support variable sized allocations, we try a set of static
        // sizes to inline small strings. All larger strings are instead allocated with an indirect
        // buffer. This can be improved when gc-arena learns to allocate variable sizes.

        fn create<'gc, const N: usize>(mc: &Mutation<'gc>, s: &[u8]) -> String<'gc> {
            #[derive(Collect)]
            #[collect(require_static)]
            #[repr(C)]
            struct InlineString<const N: usize> {
                header: Header,
                array: [u8; N],
            }

            assert!(s.len() <= N);
            let mut string = InlineString {
                header: Header {
                    hash: str_hash(&s),
                    buffer: Buffer::Inline(s.len()),
                },
                array: [0; N],
            };
            string.array[0..s.len()].copy_from_slice(s);

            let string = Gc::new(mc, string);
            // SAFETY: We know we can cast to `Header` because `InlineString` is `#[repr(C)]`
            // and `header` is the first field.
            unsafe { String(Gc::cast::<Header>(string)) }
        }

        let s = s.as_ref();

        macro_rules! try_sizes {
            ($($size:expr),*) => {
                $(if s.len() <= $size {
                    return create::<$size>(mc, s);
                })*
            };
        }
        try_sizes!(0, 2, 4, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256);

        Self::from_buffer(mc, s.into())
    }

    pub fn from_static<S: ?Sized + AsRef<[u8]>>(mc: &Mutation<'gc>, s: &'static S) -> String<'gc> {
        String(Gc::new(
            mc,
            Header {
                hash: str_hash(s.as_ref()),
                buffer: Buffer::Indirect(s.as_ref()),
            },
        ))
    }

    pub fn stored_hash(self) -> u64 {
        self.0.hash
    }

    pub fn as_bytes(self) -> &'gc [u8] {
        // SAFETY: `&'gc [u8]` has the correct lifetime because `Gc::as_ref` also returns `&'gc T`.
        unsafe {
            match self.0.buffer {
                Buffer::Indirect(p) => &(*p),
                Buffer::Inline(len) => {
                    let layout = alloc::Layout::new::<Header>();
                    let (_, offset) = layout
                        .extend(alloc::Layout::array::<u8>(len).unwrap())
                        .unwrap();
                    let data =
                        (Gc::as_ptr(self.0) as *const u8).offset(offset as isize) as *const u8;
                    slice::from_raw_parts(data, len)
                }
            }
        }
    }

    pub fn to_str(self) -> Result<&'gc str, Utf8Error> {
        str::from_utf8(self.as_bytes())
    }

    pub fn to_str_lossy(self) -> Cow<'gc, str> {
        StdString::from_utf8_lossy(self.as_bytes())
    }
}

fn str_hash(s: &[u8]) -> u64 {
    let mut state = FxHasher::default();
    state.write(s);
    state.finish()
}

#[derive(Debug, Copy, Clone, Error)]
#[error("cannot concat {bad_type}")]
pub enum StringError {
    Concat { bad_type: &'static str },
}

impl<'gc> fmt::Debug for String<'gc> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("String(")?;
        fmt.write_str(&self.to_str_lossy())?;
        fmt.write_str(")")?;
        Ok(())
    }
}

impl<'gc> fmt::Display for String<'gc> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(&self.to_str_lossy())
    }
}

impl<'gc> String<'gc> {
    pub fn concat(ctx: Context<'gc>, values: &[Value<'gc>]) -> Result<String<'gc>, StringError> {
        let mut bytes = Vec::new();
        for value in values {
            match value {
                Value::Nil => write!(&mut bytes, "nil").unwrap(),
                Value::Boolean(b) => write!(&mut bytes, "{}", b).unwrap(),
                Value::Integer(i) => write!(&mut bytes, "{}", i).unwrap(),
                Value::Number(n) => write!(&mut bytes, "{}", n).unwrap(),
                Value::String(s) => bytes.extend(s.as_bytes()),
                Value::Table(_) => return Err(StringError::Concat { bad_type: "table" }),
                Value::Function(_) => {
                    return Err(StringError::Concat {
                        bad_type: "function",
                    });
                }
                Value::Thread(_) => {
                    return Err(StringError::Concat { bad_type: "thread" });
                }
                Value::UserData(_) => {
                    return Err(StringError::Concat {
                        bad_type: "userdata",
                    });
                }
            }
        }
        Ok(ctx.intern(&bytes))
    }

    pub fn len(self) -> i64 {
        self.as_bytes().len().try_into().unwrap()
    }
}

impl<'gc> ops::Deref for String<'gc> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'gc> AsRef<[u8]> for String<'gc> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'gc, T> PartialEq<T> for String<'gc>
where
    T: ?Sized + AsRef<[u8]>,
{
    fn eq(&self, other: &T) -> bool {
        self.as_bytes() == other.as_ref()
    }
}

impl<'gc> Eq for String<'gc> {}

impl<'gc> Hash for String<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.stored_hash())
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct WeakString<'gc>(GcWeak<'gc, Header>);

impl<'gc> WeakString<'gc> {
    fn downgrade(s: String<'gc>) -> Self {
        Self(Gc::downgrade(s.0))
    }

    fn upgrade(self, mc: &Mutation<'gc>) -> Option<String<'gc>> {
        GcWeak::upgrade(self.0, mc).map(String)
    }

    fn is_dropped(self) -> bool {
        GcWeak::is_dropped(self.0)
    }
}

struct InternedDynStringsInner<'gc>(RefLock<RawTable<(WeakString<'gc>, u64), MetricsAlloc<'gc>>>);

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct InternedDynStrings<'gc>(Gc<'gc, InternedDynStringsInner<'gc>>);

unsafe impl<'gc> Collect for InternedDynStringsInner<'gc> {
    fn trace(&self, cc: &gc_arena::Collection) {
        // SAFETY: No new Gc pointers are adopted or reparented.
        let mut dyn_strings = unsafe { self.0.unlock_unchecked() }.borrow_mut();
        unsafe {
            for bucket in dyn_strings.iter() {
                let s = bucket.as_ref().0;
                s.trace(cc);
                if s.is_dropped() {
                    // SAFETY: it is okay to erase items yielded by the iterator.
                    dyn_strings.erase(bucket);
                }
            }
        }
    }
}

impl<'gc> InternedDynStrings<'gc> {
    fn new(mc: &Mutation<'gc>) -> Self {
        Self(Gc::new(
            mc,
            InternedDynStringsInner(RefLock::new(RawTable::new_in(MetricsAlloc::new(mc)))),
        ))
    }

    fn intern(self, mc: &Mutation<'gc>, s: &[u8]) -> String<'gc> {
        // SAFETY: If a new string is added, we call the write barrier.
        let mut dyn_strings = unsafe { self.0 .0.unlock_unchecked() }.borrow_mut();

        // SAFETY: The RawTable outlives the iterator
        unsafe {
            for bucket in dyn_strings.iter_hash(str_hash(s)) {
                let (key, _) = *bucket.as_ref();
                if let Some(st) = key.upgrade(mc) {
                    if st == s {
                        return st;
                    }
                } else {
                    // SAFETY: it is okay to erase items yielded by the iterator.
                    dyn_strings.erase(bucket);
                }
            }
        }

        // SAFETY: We are going to modify the dyn_strings table, so call the write barrier.
        Gc::write(mc, self.0);

        let s = String::from_slice(mc, s);
        dyn_strings.insert(
            s.stored_hash(),
            (WeakString::downgrade(s), s.stored_hash()),
            |(_, hash)| *hash,
        );

        s
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct InternedStaticStrings<'gc>(
    Gc<
        'gc,
        RefLock<
            HashMap<
                StaticCollect<*const [u8]>,
                String<'gc>,
                BuildHasherDefault<FxHasher>,
                MetricsAlloc<'gc>,
            >,
        >,
    >,
);

impl<'gc> InternedStaticStrings<'gc> {
    fn new(mc: &Mutation<'gc>) -> Self {
        Self(Gc::new(
            mc,
            RefLock::new(HashMap::with_hasher_in(
                BuildHasherDefault::default(),
                MetricsAlloc::new(mc),
            )),
        ))
    }

    fn intern(self, mc: &Mutation<'gc>, s: &'static [u8]) -> String<'gc> {
        let key = StaticCollect(s as *const _);

        // SAFETY: If a new string is added, we call the write barrier.
        let mut static_strings = unsafe { self.0.unlock_unchecked() }.borrow_mut();

        match static_strings.entry(key) {
            hash_map::Entry::Occupied(occupied) => *occupied.get(),
            hash_map::Entry::Vacant(vacant) => {
                // SAFETY: We are modifying the static_strings table, so we call the write barrier.
                Gc::write(mc, self.0);
                *vacant.insert(String::from_static(mc, s))
            }
        }
    }
}

/// A set of shared, immutable `String` values that are de-duplicated to safe space.
///
/// If the given string is the same as a previously interned string, and that interned string is
/// still "live", then a pointer to the previous string is returned instead of a newly allocated
/// string.
///
/// If there is no matching existing live interned string, then a new string is allocated.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct InternedStringSet<'gc> {
    dyn_strings: InternedDynStrings<'gc>,
    static_strings: InternedStaticStrings<'gc>,
}

impl<'gc> InternedStringSet<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> InternedStringSet<'gc> {
        InternedStringSet {
            dyn_strings: InternedDynStrings::new(mc),
            static_strings: InternedStaticStrings::new(mc),
        }
    }

    pub fn intern(self, mc: &Mutation<'gc>, s: &[u8]) -> String<'gc> {
        self.dyn_strings.intern(mc, s)
    }

    pub fn intern_static(self, mc: &Mutation<'gc>, s: &'static [u8]) -> String<'gc> {
        self.static_strings.intern(mc, s)
    }
}

#[cfg(test)]
mod tests {
    use gc_arena::rootless_arena;

    use super::*;

    #[test]
    fn test_string_header() {
        rootless_arena(|mc| {
            let test1 = String::from_buffer(mc, Box::from(b"test 1".as_slice()));
            let test2 = String::from_buffer(mc, Box::from(b"test 2".as_slice()));

            let test3 = String::from_slice(mc, b"test 3");
            let test4 = String::from_slice(mc, b"test 4444 4444 4444 4444");

            let test5 = String::from_static(mc, b"test 55555 55555 55555 55555 55555");
            let test6 = String::from_static(mc, b"test 666666");

            assert_eq!(test1.as_bytes(), b"test 1");
            assert_eq!(test2.as_bytes(), b"test 2");
            assert_eq!(test3.as_bytes(), b"test 3");
            assert_eq!(test4.as_bytes(), b"test 4444 4444 4444 4444");
            assert_eq!(test5.as_bytes(), b"test 55555 55555 55555 55555 55555");
            assert_eq!(test6.as_bytes(), b"test 666666");
        });
    }
}
