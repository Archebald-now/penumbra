#![allow(clippy::unusual_byte_groupings)]

//! Non-incremental deserialization for the [`Tree`](crate::Tree).

use futures::{stream, Stream, StreamExt};
use std::{fmt::Debug, pin::Pin};

use crate::prelude::*;
use crate::storage::{Instruction, Point, Read};

mod iresult;
// pub mod packed; // TODO: fix this module
pub use iresult::{IResult, Unexpected};

pub mod read;

use crate::internal::frontier::TrackForgotten;

/// An error when constructing something, indicative of an incorrect sequence of instructions.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Error)]
pub enum Error<E> {
    /// An unexpected instruction was provided.
    #[error(
        "instruction {instruction} building node at index {index}, height {height}: {unexpected}"
    )]
    Unexpected {
        /// The instruction at which the error occurred.
        instruction: usize,
        /// The index of the node in question.
        index: u64,
        /// The height of the node in question.
        height: u8,
        /// The unexpected instruction.
        unexpected: Unexpected,
    },
    /// Not enough instructions were supplied to construct the object.
    #[error("instruction {instruction} building node at index {index}, height {height}: not enough instructions supplied, needed at least {min_required} more")]
    Incomplete {
        /// The number of instructions supplied.
        instruction: usize,
        /// The height of the node that was currently incomplete when instructions ran out.
        height: u8,
        /// The index of the node that was currently incomplete when instructions ran out.
        index: u64,
        /// The minimum required number of instructions that would have been needed to complete construction.
        min_required: usize,
    },
    /// Too many instructions were supplied.
    #[error("instruction {instruction}: already completed construction")]
    AlreadyComplete {
        /// The number of instructions that were used to successfully construct the object.
        instruction: usize,
    },
    /// An underlying error in the stream of instructions occurred.
    #[error("{error}")]
    Underlying {
        /// The underlying error.
        #[from]
        error: E,
    },
}

type Tree = frontier::Top<frontier::Tier<frontier::Tier<frontier::Item>>>;

/// Build a tree by iterating over a sequence of [`Instruction`]s, asynchronously.
pub async fn from_instructions_stream<E>(
    position: u64,
    instructions: impl Stream<Item = Result<Instruction, E>> + Unpin,
) -> Result<crate::Tree, Error<E>> {
    let mut instructions = instructions.peekable();
    if Pin::new(&mut instructions).peek().await.is_none() {
        return Ok(crate::internal::frontier::Top::new(TrackForgotten::Yes).into());
    }

    // Count the instructions as we go along, for error reporting
    let mut instruction: usize = 0;

    // The incremental result, either an incomplete builder or a complete output
    let mut result = IResult::Incomplete(<Tree as Built>::build(position, 0));

    // For each instruction, tell the builder to use that instruction
    while let Some(this_instruction) = Pin::new(&mut instructions).next().await {
        let builder = match result {
            IResult::Complete(_) => break, // stop if complete, even if instructions aren't
            IResult::Incomplete(builder) => builder,
        };

        // Step forward the builder by one instruction
        let index = builder.index();
        let height = builder.height();
        result = builder
            .go(this_instruction?)
            .map_err(|unexpected| Error::Unexpected {
                instruction,
                unexpected,
                index,
                height,
            })?;

        // Update the instruction count
        instruction += 1;
    }

    // Examine whether we successfully constructed the tree
    match result {
        // If complete, return the output tree
        IResult::Complete(output) => {
            // Ensure that no more instructions are remaining
            if Pin::new(&mut instructions).peek().await.is_some() {
                return Err(Error::AlreadyComplete { instruction });
            }
            Ok(output.into())
        }
        // If incomplete, return an error indicating the situation we stopped in
        IResult::Incomplete(builder) => Err(Error::Incomplete {
            instruction,
            height: builder.height(),
            index: builder.index(),
            min_required: builder.min_required(),
        }),
    }
}

/// Build a tree by iterating over a sequence of [`Instruction`]s, synchronously.
pub fn from_instructions_iter<E>(
    position: u64,
    instructions: impl IntoIterator<Item = Result<Instruction, E>> + Unpin,
) -> Result<crate::Tree, Error<E>> {
    let future = from_instructions_stream(position, stream::iter(instructions.into_iter()));
    futures::executor::block_on(future)
}

/// Build a tree by iterating over a stream of (position, depth) pairs, asynchronously.
///
/// # Errors
///
/// The stream of points must be in lexicographic order by (position, depth), and the instruction
/// stream represented by the points must be a valid pre-order depth-first traversal of some
/// [`Tree`]. Otherwise, an error will be thrown.
pub async fn from_points<E>(
    position: u64,
    points: impl Stream<Item = Result<Point, E>> + Unpin,
) -> Result<crate::Tree, Error<read::Error<E>>> {
    from_instructions_stream(position, read::Reader::new(points).stream()).await
}

/// Build a tree from a reader that provides an enumeration of the points stored.
pub async fn from_reader<R: Read>(
    reader: &mut R,
) -> Result<crate::Tree, Error<read::Error<R::Error>>> {
    let position = reader
        .position()
        .await
        .map_err(|error| read::Error::Underlying { error })?;
    from_points(position, reader.points()).await
}