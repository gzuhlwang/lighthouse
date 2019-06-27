#![allow(clippy::range_plus_one)] // Minor readability lint requiring structural changes; not worth it.

use super::*;
use crate::merkleize::{merkleize, pad_for_leaf_count};
use int_to_bytes::int_to_bytes32;
use ssz_derive::{Decode, Encode};

/// Provides cached tree hashing for some object implementing `CachedTreeHash`.
///
/// Caching allows for doing minimal internal-node hashing when an object has only been partially
/// changed.
///
/// See the crate root for an example.
#[derive(Debug, PartialEq, Clone, Encode, Decode)]
pub struct TreeHashCache {
    /// Stores the binary-tree in 32-byte chunks.
    pub bytes: Vec<u8>,
    /// Maps to each chunk of `self.bytes`, indicating if the chunk is dirty.
    pub chunk_modified: Vec<bool>,
    /// Contains a schema for each variable-length item stored in the cache.
    pub schemas: Vec<BTreeSchema>,

    /// A counter used during updates.
    pub chunk_index: usize,
    /// A counter used during updates.
    pub schema_index: usize,
}

impl Default for TreeHashCache {
    /// Create an empty cache.
    ///
    /// Note: an empty cache is effectively useless, an error will be raised if `self.update` is
    /// called.
    fn default() -> TreeHashCache {
        TreeHashCache {
            bytes: vec![],
            chunk_modified: vec![],
            schemas: vec![],
            chunk_index: 0,
            schema_index: 0,
        }
    }
}

impl TreeHashCache {
    /// Instantiates a new cache from `item` at a depth of `0`.
    ///
    /// The returned cache is fully-built and will return an accurate tree-hash root.
    pub fn new<T>(item: &T) -> Result<Self, Error>
    where
        T: CachedTreeHash,
    {
        Self::new_at_depth(item, 0)
    }

    /// Instantiates a new cache from `item` at the specified `depth`.
    ///
    /// The returned cache is fully-built and will return an accurate tree-hash root.
    pub fn new_at_depth<T>(item: &T, depth: usize) -> Result<Self, Error>
    where
        T: CachedTreeHash,
    {
        item.new_tree_hash_cache(depth)
    }

    /// Updates the cache with `item`.
    ///
    /// `item` _must_ be of the same type as the `item` used to build the cache, otherwise an error
    /// may be returned.
    ///
    /// After calling `update`, the cache will return an accurate tree-hash root using
    /// `self.tree_hash_root()`.
    pub fn update<T>(&mut self, item: &T) -> Result<(), Error>
    where
        T: CachedTreeHash,
    {
        if self.is_empty() {
            Err(Error::CacheNotInitialized)
        } else {
            self.reset_modifications();

            item.update_tree_hash_cache(self)
        }
    }

    /// Builds a new cache for `item`, given `subtrees` contains a `Self` for field/item of `item`.
    ///
    /// Each `subtree` in `subtree` will become a leaf-node of the merkle-tree of `item`.
    pub fn from_subtrees<T>(item: &T, subtrees: Vec<Self>, depth: usize) -> Result<Self, Error>
    where
        T: CachedTreeHash,
    {
        let overlay = BTreeOverlay::new(item, 0, depth);

        // Note how many leaves were provided. If is not a power-of-two, we'll need to pad it out
        // later.
        let num_provided_leaf_nodes = subtrees.len();

        // Allocate enough bytes to store the internal nodes and the leaves and subtrees, then fill
        // all the to-be-built internal nodes with zeros and append the leaves and subtrees.
        let internal_node_bytes = overlay.num_internal_nodes() * BYTES_PER_CHUNK;
        let subtrees_bytes = subtrees.iter().fold(0, |acc, t| acc + t.bytes.len());
        let mut bytes = Vec::with_capacity(subtrees_bytes + internal_node_bytes);
        bytes.resize(internal_node_bytes, 0);

        // Allocate enough bytes to store all the leaves.
        let mut leaves = Vec::with_capacity(overlay.num_leaf_nodes() * HASHSIZE);
        let mut schemas = Vec::with_capacity(subtrees.len());

        if T::tree_hash_type() == TreeHashType::List {
            schemas.push(overlay.into());
        }

        // Iterate through all of the leaves/subtrees, adding their root as a leaf node and then
        // concatenating their merkle trees.
        for t in subtrees {
            leaves.append(&mut t.tree_hash_root()?.to_vec());

            let (mut t_bytes, _bools, mut t_schemas) = t.into_components();
            bytes.append(&mut t_bytes);
            schemas.append(&mut t_schemas);
        }

        // Pad the leaves to an even power-of-two, using zeros.
        pad_for_leaf_count(num_provided_leaf_nodes, &mut bytes);

        // Merkleize the leaves, then split the leaf nodes off them. Then, replace all-zeros
        // internal nodes created earlier with the internal nodes generated by `merkleize`.
        let mut merkleized = merkleize(leaves);
        merkleized.split_off(internal_node_bytes);
        bytes.splice(0..internal_node_bytes, merkleized);

        Ok(Self {
            chunk_modified: vec![true; bytes.len() / BYTES_PER_CHUNK],
            bytes,
            schemas,
            chunk_index: 0,
            schema_index: 0,
        })
    }

    /// Instantiate a new cache from the pre-built `bytes` where each `self.chunk_modified` will be
    /// set to `intitial_modified_state`.
    ///
    /// Note: `bytes.len()` must be a multiple of 32
    pub fn from_bytes(
        bytes: Vec<u8>,
        initial_modified_state: bool,
        schema: Option<BTreeSchema>,
    ) -> Result<Self, Error> {
        if bytes.len() % BYTES_PER_CHUNK > 0 {
            return Err(Error::BytesAreNotEvenChunks(bytes.len()));
        }

        let schemas = match schema {
            Some(schema) => vec![schema],
            None => vec![],
        };

        Ok(Self {
            chunk_modified: vec![initial_modified_state; bytes.len() / BYTES_PER_CHUNK],
            bytes,
            schemas,
            chunk_index: 0,
            schema_index: 0,
        })
    }

    /// Returns `true` if this cache is empty (i.e., it has never been built for some item).
    ///
    /// Note: an empty cache is effectively useless, an error will be raised if `self.update` is
    /// called.
    pub fn is_empty(&self) -> bool {
        self.chunk_modified.is_empty()
    }

    /// Return an overlay, built from the schema at `schema_index` with an offset of `chunk_index`.
    pub fn get_overlay(
        &self,
        schema_index: usize,
        chunk_index: usize,
    ) -> Result<BTreeOverlay, Error> {
        Ok(self
            .schemas
            .get(schema_index)
            .ok_or_else(|| Error::NoSchemaForIndex(schema_index))?
            .clone()
            .into_overlay(chunk_index))
    }

    /// Resets the per-update counters, allowing a new update to start.
    ///
    /// Note: this does _not_ delete the contents of the cache.
    pub fn reset_modifications(&mut self) {
        // Reset the per-hash counters.
        self.chunk_index = 0;
        self.schema_index = 0;

        for chunk_modified in &mut self.chunk_modified {
            *chunk_modified = false;
        }
    }

    /// Replace the schema at `schema_index` with the schema derived from `new_overlay`.
    ///
    /// If the `new_overlay` schema has a different number of internal nodes to the schema at
    /// `schema_index`, the cache will be updated to add/remove these new internal nodes.
    pub fn replace_overlay(
        &mut self,
        schema_index: usize,
        // TODO: remove chunk index (if possible)
        chunk_index: usize,
        new_overlay: BTreeOverlay,
    ) -> Result<BTreeOverlay, Error> {
        let old_overlay = self.get_overlay(schema_index, chunk_index)?;
        // If the merkle tree required to represent the new list is of a different size to the one
        // required for the previous list, then update the internal nodes.
        //
        // Leaf nodes are not touched, they should be updated externally to this function.
        //
        // This grows/shrinks the bytes to accommodate the new tree, preserving as much of the tree
        // as possible.
        if new_overlay.num_internal_nodes() != old_overlay.num_internal_nodes() {
            // Get slices of the existing tree from the cache.
            let (old_bytes, old_flags) = self
                .slices(old_overlay.internal_chunk_range())
                .ok_or_else(|| Error::UnableToObtainSlices)?;

            let (new_bytes, new_flags) = if new_overlay.num_internal_nodes() == 0 {
                // The new tree has zero internal nodes, simply return empty lists.
                (vec![], vec![])
            } else if old_overlay.num_internal_nodes() == 0 {
                // The old tree has zero nodes and the new tree has some nodes. Create new nodes to
                // suit.
                let nodes = resize::nodes_in_tree_of_height(new_overlay.height() - 1);

                (vec![0; nodes * HASHSIZE], vec![true; nodes])
            } else if new_overlay.num_internal_nodes() > old_overlay.num_internal_nodes() {
                // The new tree is bigger than the old tree.
                //
                // Grow the internal nodes, preserving any existing nodes.
                resize::grow_merkle_tree(
                    old_bytes,
                    old_flags,
                    old_overlay.height() - 1,
                    new_overlay.height() - 1,
                )
                .ok_or_else(|| Error::UnableToGrowMerkleTree)?
            } else {
                // The new tree is smaller than the old tree.
                //
                // Shrink the internal nodes, preserving any existing nodes.
                resize::shrink_merkle_tree(
                    old_bytes,
                    old_flags,
                    old_overlay.height() - 1,
                    new_overlay.height() - 1,
                )
                .ok_or_else(|| Error::UnableToShrinkMerkleTree)?
            };

            // Splice the resized created elements over the existing elements, effectively updating
            // the number of stored internal nodes for this tree.
            self.splice(old_overlay.internal_chunk_range(), new_bytes, new_flags);
        }

        let old_schema = std::mem::replace(&mut self.schemas[schema_index], new_overlay.into());

        Ok(old_schema.into_overlay(chunk_index))
    }

    /// Remove all of the child schemas following `schema_index`.
    ///
    /// Schema `a` is a child of schema `b` if `a.depth < b.depth`.
    pub fn remove_proceeding_child_schemas(&mut self, schema_index: usize, depth: usize) {
        let end = self
            .schemas
            .iter()
            .skip(schema_index)
            .position(|o| o.depth <= depth)
            .and_then(|i| Some(i + schema_index))
            .unwrap_or_else(|| self.schemas.len());

        self.schemas.splice(schema_index..end, vec![]);
    }

    /// Iterate through the internal nodes chunks of `overlay`, updating the chunk with the
    /// merkle-root of it's children if either of those children are dirty.
    pub fn update_internal_nodes(&mut self, overlay: &BTreeOverlay) -> Result<(), Error> {
        for (parent, children) in overlay.internal_parents_and_children().into_iter().rev() {
            if self.either_modified(children)? {
                self.modify_chunk(parent, &self.hash_children(children)?)?;
            }
        }

        Ok(())
    }

    /// Returns to the tree-hash root of the cache.
    pub fn tree_hash_root(&self) -> Result<&[u8], Error> {
        if self.is_empty() {
            Err(Error::CacheNotInitialized)
        } else {
            self.bytes
                .get(0..HASHSIZE)
                .ok_or_else(|| Error::NoBytesForRoot)
        }
    }

    /// Splices the given `bytes` over `self.bytes` and `bools` over `self.chunk_modified` at the
    /// specified `chunk_range`.
    pub fn splice(&mut self, chunk_range: Range<usize>, bytes: Vec<u8>, bools: Vec<bool>) {
        // Update the `chunk_modified` vec, marking all spliced-in nodes as changed.
        self.chunk_modified.splice(chunk_range.clone(), bools);
        self.bytes
            .splice(node_range_to_byte_range(&chunk_range), bytes);
    }

    /// If the bytes at `chunk` are not the same as `to`, `self.bytes` is updated and
    /// `self.chunk_modified` is set to `true`.
    pub fn maybe_update_chunk(&mut self, chunk: usize, to: &[u8]) -> Result<(), Error> {
        let start = chunk * BYTES_PER_CHUNK;
        let end = start + BYTES_PER_CHUNK;

        if !self.chunk_equals(chunk, to)? {
            self.bytes
                .get_mut(start..end)
                .ok_or_else(|| Error::NoModifiedFieldForChunk(chunk))?
                .copy_from_slice(to);
            self.chunk_modified[chunk] = true;
        }

        Ok(())
    }

    /// Returns the slices of `self.bytes` and `self.chunk_modified` at the given `chunk_range`.
    fn slices(&self, chunk_range: Range<usize>) -> Option<(&[u8], &[bool])> {
        Some((
            self.bytes.get(node_range_to_byte_range(&chunk_range))?,
            self.chunk_modified.get(chunk_range)?,
        ))
    }

    /// Updates `self.bytes` at `chunk` and sets `self.chunk_modified` for the `chunk` to `true`.
    pub fn modify_chunk(&mut self, chunk: usize, to: &[u8]) -> Result<(), Error> {
        let start = chunk * BYTES_PER_CHUNK;
        let end = start + BYTES_PER_CHUNK;

        self.bytes
            .get_mut(start..end)
            .ok_or_else(|| Error::NoBytesForChunk(chunk))?
            .copy_from_slice(to);

        self.chunk_modified[chunk] = true;

        Ok(())
    }

    /// Returns the bytes at `chunk`.
    fn get_chunk(&self, chunk: usize) -> Result<&[u8], Error> {
        let start = chunk * BYTES_PER_CHUNK;
        let end = start + BYTES_PER_CHUNK;

        Ok(self
            .bytes
            .get(start..end)
            .ok_or_else(|| Error::NoModifiedFieldForChunk(chunk))?)
    }

    /// Returns `true` if the bytes at `chunk` are equal to `other`.
    fn chunk_equals(&mut self, chunk: usize, other: &[u8]) -> Result<bool, Error> {
        Ok(self.get_chunk(chunk)? == other)
    }

    /// Returns `true` if `chunk` is dirty.
    pub fn changed(&self, chunk: usize) -> Result<bool, Error> {
        self.chunk_modified
            .get(chunk)
            .cloned()
            .ok_or_else(|| Error::NoModifiedFieldForChunk(chunk))
    }

    /// Returns `true` if either of the `children` chunks is dirty.
    fn either_modified(&self, children: (usize, usize)) -> Result<bool, Error> {
        Ok(self.changed(children.0)? | self.changed(children.1)?)
    }

    /// Returns the hash of the concatenation of the given `children`.
    pub fn hash_children(&self, children: (usize, usize)) -> Result<Vec<u8>, Error> {
        let mut child_bytes = Vec::with_capacity(BYTES_PER_CHUNK * 2);
        child_bytes.append(&mut self.get_chunk(children.0)?.to_vec());
        child_bytes.append(&mut self.get_chunk(children.1)?.to_vec());

        Ok(hash(&child_bytes))
    }

    /// Adds a chunk before and after the given `chunk` range and calls `self.mix_in_length()`.
    pub fn add_length_nodes(
        &mut self,
        chunk_range: Range<usize>,
        length: usize,
    ) -> Result<(), Error> {
        self.chunk_modified[chunk_range.start] = true;

        let byte_range = node_range_to_byte_range(&chunk_range);

        // Add the last node.
        self.bytes
            .splice(byte_range.end..byte_range.end, vec![0; HASHSIZE]);
        self.chunk_modified
            .splice(chunk_range.end..chunk_range.end, vec![false]);

        // Add the first node.
        self.bytes
            .splice(byte_range.start..byte_range.start, vec![0; HASHSIZE]);
        self.chunk_modified
            .splice(chunk_range.start..chunk_range.start, vec![false]);

        self.mix_in_length(chunk_range.start + 1..chunk_range.end + 1, length)?;

        Ok(())
    }

    /// Sets `chunk_range.end + 1` equal to the little-endian serialization of `length`. Sets
    /// `chunk_range.start - 1` equal to `self.hash_children(chunk_range.start, chunk_range.end + 1)`.
    pub fn mix_in_length(&mut self, chunk_range: Range<usize>, length: usize) -> Result<(), Error> {
        // Update the length chunk.
        self.maybe_update_chunk(chunk_range.end, &int_to_bytes32(length as u64))?;

        // Update the mixed-in root if the main root or the length have changed.
        let children = (chunk_range.start, chunk_range.end);
        if self.either_modified(children)? {
            self.modify_chunk(chunk_range.start - 1, &self.hash_children(children)?)?;
        }

        Ok(())
    }

    /// Returns `(self.bytes, self.chunk_modified, self.schemas)`.
    pub fn into_components(self) -> (Vec<u8>, Vec<bool>, Vec<BTreeSchema>) {
        (self.bytes, self.chunk_modified, self.schemas)
    }
}

fn node_range_to_byte_range(node_range: &Range<usize>) -> Range<usize> {
    node_range.start * HASHSIZE..node_range.end * HASHSIZE
}
