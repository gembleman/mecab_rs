use std::num::NonZero;
use std::ops::{Index, IndexMut};

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) struct $name(NonZero<u32>);

        impl $name {
            pub(crate) fn from_index(index: usize) -> Self {
                Self(NonZero::new(index as u32 + 1).unwrap())
            }

            fn index(self) -> usize {
                self.0.get() as usize - 1
            }
        }

        const _: () = assert!(size_of::<Option<$name>>() == 4);

        impl<T> Index<$name> for [T] {
            type Output = T;

            fn index(&self, id: $name) -> &T {
                &self[id.index()]
            }
        }

        impl<T> IndexMut<$name> for [T] {
            fn index_mut(&mut self, id: $name) -> &mut T {
                &mut self[id.index()]
            }
        }

        impl<T> Index<$name> for Vec<T> {
            type Output = T;

            fn index(&self, id: $name) -> &T {
                &self[id.index()]
            }
        }

        impl<T> IndexMut<$name> for Vec<T> {
            fn index_mut(&mut self, id: $name) -> &mut T {
                &mut self[id.index()]
            }
        }
    };
}

define_id!(NodeId);
define_id!(LinkId);
define_id!(ElementId);
