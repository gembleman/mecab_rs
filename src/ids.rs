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

            pub(crate) fn index(self) -> usize {
                self.0.get() as usize - 1
            }
        }

        const _: () = assert!(size_of::<Option<$name>>() == 4);
    };
}

macro_rules! index_by {
    ($name:ident, $container:ty) => {
        impl<T> Index<$name> for $container {
            type Output = T;

            fn index(&self, id: $name) -> &T {
                &self[id.index()]
            }
        }
    };
}

macro_rules! index_mut_by {
    ($name:ident, $container:ty) => {
        impl<T> IndexMut<$name> for $container {
            fn index_mut(&mut self, id: $name) -> &mut T {
                &mut self[id.index()]
            }
        }
    };
}

define_id!(NodeId);
define_id!(ElementId);

index_by!(NodeId, [T]);
index_by!(NodeId, Vec<T>);
index_mut_by!(NodeId, Vec<T>);
index_by!(ElementId, Vec<T>);
