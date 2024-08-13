//! `ComponentChanges` implementation.

use std::{cell::UnsafeCell, fmt::Debug, marker::PhantomData, str::FromStr, sync::OnceLock};

use ahash::AHashMap;
use bytes::{Buf, BufMut};
use edcode2::{BufExt as _, BufMutExt as _, Decode, Encode};
use rimecraft_global_cx::ProvideIdTy;
use rimecraft_maybe::{Maybe, SimpleOwned};
use rimecraft_registry::{ProvideRegistry, Reg};
use serde::{de::DeserializeSeed, ser::SerializeMap, Deserialize, Serialize};

use crate::{
    map::CompTyCell, ComponentType, ErasedComponentType, Object, RawErasedComponentType,
    UnsafeDebugIter, UnsafeSerdeCodec,
};

/// Changes of components.
pub struct ComponentChanges<'a, 'cow, Cx>
where
    Cx: ProvideIdTy,
{
    pub(crate) changed: Maybe<'cow, AHashMap<CompTyCell<'a, Cx>, Option<Box<Object<'a>>>>>,
    pub(crate) ser_count: usize,
}

impl<'a, Cx> ComponentChanges<'a, '_, Cx>
where
    Cx: ProvideIdTy,
{
    /// Returns a builder for `ComponentChanges`.
    pub fn builder() -> Builder<'a, Cx> {
        Builder {
            changes: AHashMap::new(),
            ser_count: 0,
        }
    }

    /// Gets the component with given type.
    ///
    /// # Safety
    ///
    /// This function could not guarantee lifetime of type `T` is sound.
    /// The type `T`'s lifetime parameters should not overlap lifetime `'a`.
    pub unsafe fn get<T: 'a>(&self, ty: &ComponentType<'a, T>) -> Option<Option<&T>> {
        let val = self.get_raw(&RawErasedComponentType::from(ty))?;
        if let Some(val) = val {
            let downcasted = val.downcast_ref::<T>()?;
            Some(Some(downcasted))
        } else {
            Some(None)
        }
    }

    #[inline]
    fn get_raw(&self, ty: &RawErasedComponentType<'a, Cx>) -> Option<Option<&Object<'_>>> {
        self.changed.get(ty).map(Option::as_deref)
    }

    /// Returns number of changed components.
    #[inline]
    pub fn len(&self) -> usize {
        self.changed.len()
    }

    /// Returns `true` if there are no changed components.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }
}

impl<Cx> Serialize for ComponentChanges<'_, '_, Cx>
where
    Cx: ProvideIdTy,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.ser_count))?;
        for (&CompTyCell(ty), obj) in self.changed.iter().filter(|(k, _)| k.0.is_serializable()) {
            struct Ser<'a, 's> {
                obj: &'s Object<'a>,
                codec: &'a UnsafeSerdeCodec<'a>,
            }

            impl Serialize for Ser<'_, '_> {
                #[inline]
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: serde::Serializer,
                {
                    (self.codec.ser)(self.obj).serialize(serializer)
                }
            }

            let ty = Type {
                ty,
                rm: obj.is_none(),
                cached_ser: OnceLock::new(),
            };

            map.serialize_key(&ty)?;
            if obj.is_none() {
                // Dummy value. fastnbt does not support Unit values.
                map.serialize_value(&0u32)?;
            } else {
                map.serialize_value(&Ser {
                    obj: obj.as_ref().unwrap(),
                    codec: ty.ty.f.serde_codec.expect("missing serde codec"),
                })?;
            }
        }
        map.end()
    }
}

impl<'a, 'de, Cx> Deserialize<'de> for ComponentChanges<'a, '_, Cx>
where
    Cx: ProvideIdTy<Id: FromStr> + ProvideRegistry<'a, Cx::Id, RawErasedComponentType<'a, Cx>>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<'a, Cx>(PhantomData<(Cx, &'a ())>);

        impl<'a, 'de, Cx> serde::de::Visitor<'de> for Visitor<'a, Cx>
        where
            Cx: ProvideIdTy<Id: FromStr>
                + ProvideRegistry<'a, Cx::Id, RawErasedComponentType<'a, Cx>>,
        {
            type Value = AHashMap<CompTyCell<'a, Cx>, Option<Box<Object<'a>>>>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "a map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut changes;

                if let Some(hint) = map.size_hint() {
                    changes = AHashMap::with_capacity(hint);
                } else {
                    changes = AHashMap::new();
                }

                while let Some(ty) = map.next_key::<Type<'a, Cx>>()? {
                    if ty.rm {
                        // Skips a dummy value. fastnbt does not support Unit values.
                        let _: () = map.next_value()?;
                        changes.insert(CompTyCell(ty.ty), None);
                    } else {
                        struct Seed<'a>(&'a UnsafeSerdeCodec<'a>);
                        impl<'de, 'a> DeserializeSeed<'de> for Seed<'a> {
                            type Value = Box<Object<'a>>;

                            fn deserialize<D>(
                                self,
                                deserializer: D,
                            ) -> Result<Self::Value, D::Error>
                            where
                                D: serde::Deserializer<'de>,
                            {
                                (self.0.de)(&mut <dyn erased_serde::Deserializer<'de>>::erase(
                                    deserializer,
                                ))
                                .map_err(serde::de::Error::custom)
                            }
                        }
                        changes.insert(
                            CompTyCell(ty.ty),
                            Some(map.next_value_seed(Seed(
                                ty.ty.f.serde_codec.expect("missing serde codec"),
                            ))?),
                        );
                    }
                }

                Ok(changes)
            }
        }

        deserializer
            .deserialize_map(Visitor(PhantomData))
            .map(|changed| ComponentChanges {
                ser_count: changed.len(),
                changed: Maybe::Owned(SimpleOwned(changed)),
            })
    }
}

impl<Cx, B> Encode<B> for ComponentChanges<'_, '_, Cx>
where
    Cx: ProvideIdTy,
    B: BufMut,
{
    fn encode(&self, mut buf: B) -> Result<(), edcode2::BoxedError<'static>> {
        let present = self.changed.values().filter(|val| val.is_some()).count() as u32;
        buf.put_variable(present);
        buf.put_variable(self.changed.len() as u32 - present);

        for (&CompTyCell(ty), val) in self.changed.iter() {
            if let Some(val) = val {
                buf.put_variable(1);
                ty.encode(&mut buf)?;
                (ty.f.packet_codec.encode)(&**val, &mut buf)?;
            }
        }
        for (&CompTyCell(ty), val) in self.changed.iter() {
            if val.is_none() {
                ty.encode(&mut buf)?;
            }
        }

        Ok(())
    }
}

impl<'a: 'de, 'de, Cx, B> Decode<'de, B> for ComponentChanges<'a, '_, Cx>
where
    Cx: ProvideIdTy<Id: for<'b> Decode<'de, &'b mut B>>
        + ProvideRegistry<'a, Cx::Id, RawErasedComponentType<'a, Cx>>,
    B: Buf,
{
    fn decode(mut buf: B) -> Result<Self, edcode2::BoxedError<'de>> {
        let present = buf.get_variable::<u32>();
        let absent = buf.get_variable::<u32>();
        let len = (present + absent) as usize;

        let mut changed = AHashMap::with_capacity(len);
        for _ in 0..present {
            let ty = ErasedComponentType::decode(&mut buf)?;
            let obj = (ty.f.packet_codec.decode)(&mut buf)?;
            changed.insert(CompTyCell(ty), Some(obj));
        }
        for _ in 0..absent {
            let ty = ErasedComponentType::decode(&mut buf)?;
            changed.insert(CompTyCell(ty), None);
        }

        Ok(ComponentChanges {
            ser_count: changed.keys().filter(|k| k.0.is_serializable()).count(),
            changed: Maybe::Owned(SimpleOwned(changed)),
        })
    }
}

/// Builder for [`ComponentChanges`].
pub struct Builder<'a, Cx>
where
    Cx: ProvideIdTy,
{
    changes: AHashMap<CompTyCell<'a, Cx>, Option<Box<Object<'a>>>>,
    ser_count: usize,
}

impl<'a, Cx> Builder<'a, Cx>
where
    Cx: ProvideIdTy,
{
    /// Inserts a component type with a valid value.
    ///
    /// # Panics
    ///
    /// Panics if the type of the value does not match the component type.
    #[inline]
    pub fn insert<T>(&mut self, ty: ErasedComponentType<'a, Cx>, value: T)
    where
        T: Send + Sync + 'a,
    {
        assert_eq!(
            ty.ty,
            typeid::of::<T>(),
            "the type {} does not match the component type",
            std::any::type_name::<T>()
        );
        self.changes.insert(CompTyCell(ty), Some(Box::new(value)));
        if ty.is_serializable() {
            self.ser_count += 1;
        }
    }

    /// Inserts a component type with an empty value.
    #[inline]
    pub fn remove(&mut self, ty: ErasedComponentType<'a, Cx>) {
        self.changes.insert(CompTyCell(ty), None);
    }

    /// Builds the changes into a [`ComponentChanges`].
    #[inline]
    pub fn build<'cow>(self) -> ComponentChanges<'a, 'cow, Cx> {
        ComponentChanges {
            changed: Maybe::Owned(SimpleOwned(self.changes)),
            ser_count: self.ser_count,
        }
    }
}

impl<'a, Cx> From<Builder<'a, Cx>> for ComponentChanges<'a, '_, Cx>
where
    Cx: ProvideIdTy,
{
    #[inline]
    fn from(builder: Builder<'a, Cx>) -> Self {
        builder.build()
    }
}

const REMOVED_PREFIX: char = '!';

struct Type<'a, Cx>
where
    Cx: ProvideIdTy,
{
    ty: ErasedComponentType<'a, Cx>,
    rm: bool,

    cached_ser: OnceLock<String>,
}

impl<Cx> Serialize for Type<'_, Cx>
where
    Cx: ProvideIdTy,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.cached_ser.get_or_init(|| {
            let id = Reg::id(self.ty);
            if self.rm {
                format!("{}{}", REMOVED_PREFIX, id)
            } else {
                id.to_string()
            }
        }))
    }
}

impl<'a, 'de, Cx> Deserialize<'de> for Type<'a, Cx>
where
    Cx: ProvideIdTy + ProvideRegistry<'a, Cx::Id, RawErasedComponentType<'a, Cx>>,
    Cx::Id: FromStr,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<'a, Cx>
        where
            Cx: ProvideIdTy,
        {
            _marker: PhantomData<&'a Cx>,
        }

        impl<'a, Cx> serde::de::Visitor<'_> for Visitor<'a, Cx>
        where
            Cx: ProvideIdTy + ProvideRegistry<'a, Cx::Id, RawErasedComponentType<'a, Cx>>,
            Cx::Id: FromStr,
        {
            type Value = Type<'a, Cx>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "a string")
            }

            #[inline]
            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let stripped = value.strip_prefix(REMOVED_PREFIX);
                let any = stripped.unwrap_or(value);
                let id: Cx::Id = any.parse().ok().ok_or_else(|| {
                    E::custom(format!("unable to deserialize the identifier {}", any))
                })?;

                let ty = Cx::registry().get(&id).ok_or_else(|| {
                    E::custom(format!("unable to find the component type {}", id))
                })?;

                if !ty.is_serializable() {
                    return Err(E::custom(format!(
                        "the component type {} is not serializable",
                        id
                    )));
                }

                Ok(Type {
                    ty,
                    rm: stripped.is_some(),

                    cached_ser: OnceLock::new(),
                })
            }
        }

        deserializer.deserialize_str(Visitor {
            _marker: PhantomData,
        })
    }
}

impl<Cx> Debug for ComponentChanges<'_, '_, Cx>
where
    Cx: ProvideIdTy + Debug,
    Cx::Id: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&UnsafeDebugIter(UnsafeCell::new(self.changed.keys())), f)
    }
}

impl<Cx> Debug for Builder<'_, Cx>
where
    Cx: ProvideIdTy + Debug,
    Cx::Id: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&UnsafeDebugIter(UnsafeCell::new(self.changes.keys())), f)
    }
}
