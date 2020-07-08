use crate::prelude::*;
use crossbeam::thread;
use fnv::{FnvBuildHasher, FnvHashMap};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

macro_rules! hash_join_inner {
    ($s_right:ident, $ca_left:ident, $type_:ident) => {{
        // call the type method series.i32()
        let ca_right = $s_right.$type_()?;
        $ca_left.hash_join_inner(ca_right)
    }};
}

macro_rules! hash_join_left {
    ($s_right:ident, $ca_left:ident, $type_:ident) => {{
        // call the type method series.i32()
        let ca_right = $s_right.$type_()?;
        $ca_left.hash_join_left(ca_right)
    }};
}

macro_rules! hash_join_outer {
    ($s_right:ident, $ca_left:ident, $type_:ident) => {{
        // call the type method series.i32()
        let ca_right = $s_right.$type_()?;
        $ca_left.hash_join_outer(ca_right)
    }};
}

macro_rules! apply_hash_join_on_series {
    ($s_left:ident, $s_right:ident, $join_macro:ident) => {{
        match $s_left {
            Series::UInt32(ca_left) => $join_macro!($s_right, ca_left, u32),
            Series::Int32(ca_left) => $join_macro!($s_right, ca_left, i32),
            Series::Int64(ca_left) => $join_macro!($s_right, ca_left, i64),
            Series::Bool(ca_left) => $join_macro!($s_right, ca_left, bool),
            Series::Utf8(ca_left) => $join_macro!($s_right, ca_left, utf8),
            _ => unimplemented!(),
        }
    }};
}

pub(crate) fn prepare_hashed_relation<T>(
    b: impl Iterator<Item = T>,
) -> HashMap<T, Vec<usize>, FnvBuildHasher>
where
    T: Hash + Eq + Copy,
{
    let mut hash_tbl = FnvHashMap::default();

    b.enumerate()
        .for_each(|(idx, key)| hash_tbl.entry(key).or_insert_with(Vec::new).push(idx));
    hash_tbl
}

/// Hash join a and b.
///     b should be the shorter relation.
/// NOTE that T also can be an Option<T>. Nulls are seen as equal.
fn hash_join_tuples_inner<T>(
    a: impl Iterator<Item = T>,
    b: impl Iterator<Item = T>,
    // Because b should be the shorter relation we could need to swap to keep left left and right right.
    swap: bool,
) -> Vec<(usize, usize)>
where
    T: Hash + Eq + Copy,
{
    let mut results = Vec::new();
    // First we hash one relation
    let hash_tbl = prepare_hashed_relation(b);

    // Next we probe the other relation in the hash table
    a.enumerate().for_each(|(idx_a, key)| {
        if let Some(indexes_b) = hash_tbl.get(&key) {
            let tuples = indexes_b
                .iter()
                .map(|&idx_b| if swap { (idx_b, idx_a) } else { (idx_a, idx_b) });
            results.extend(tuples)
        }
    });
    results
}

/// Hash join left. None/ Nulls are regarded as Equal
/// All left values are joined so no Option<usize> there.
fn hash_join_tuples_left<T>(
    a: impl Iterator<Item = T>,
    b: impl Iterator<Item = T>,
) -> Vec<(usize, Option<usize>)>
where
    T: Hash + Eq + Copy,
{
    let mut results = Vec::new();
    // First we hash one relation
    let hash_tbl = prepare_hashed_relation(b);

    // Next we probe the other relation in the hash table
    a.enumerate().for_each(|(idx_a, key)| {
        match hash_tbl.get(&key) {
            // left and right matches
            Some(indexes_b) => results.extend(indexes_b.iter().map(|&idx_b| (idx_a, Some(idx_b)))),
            // only left values, right = null
            None => results.push((idx_a, None)),
        }
    });
    results
}

/// Hash join outer. Both left and right can have no match so Options
/// We accept a closure as we need to do two passes over the same iterators.
fn hash_join_tuples_outer<'a, T, I, J>(
    a: I,
    b: J,
    capacity: usize,
) -> HashSet<(Option<usize>, Option<usize>), FnvBuildHasher>
where
    I: Fn() -> Box<dyn Iterator<Item = T> + 'a> + Sync,
    J: Fn() -> Box<dyn Iterator<Item = T> + 'a> + Sync,
    T: Hash + Eq + Copy + Sync,
{
    let results =
        thread::scope(|s| {
            let handle_left = s.spawn(|_| {
                let mut results =
                    HashSet::with_capacity_and_hasher(capacity, FnvBuildHasher::default());

                // We do the hash probe combination on both relations.
                let hash_tbl = prepare_hashed_relation(b());

                a().enumerate().for_each(|(idx_a, key)| {
                    match hash_tbl.get(&key) {
                        // left and right matches
                        Some(indexes_b) => results
                            .extend(indexes_b.iter().map(|&idx_b| (Some(idx_a), Some(idx_b)))),
                        // only left values, right = null
                        None => {
                            results.insert((Some(idx_a), None));
                        }
                    }
                });
                results
            });

            let handle_right = s.spawn(|_| {
                let mut results =
                    HashSet::with_capacity_and_hasher(capacity, FnvBuildHasher::default());
                let hash_tbl = prepare_hashed_relation(a());

                b().enumerate().for_each(|(idx_b, key)| {
                    match hash_tbl.get(&key) {
                        // left and right matches
                        Some(indexes_a) => results
                            .extend(indexes_a.iter().map(|&idx_a| (Some(idx_a), Some(idx_b)))),
                        // only left values, right = null
                        None => {
                            results.insert((None, Some(idx_b)));
                        }
                    }
                });
                results
            });

            let mut results_left = handle_left.join().expect("could not join threads");
            let results_right = handle_right.join().expect("could not join threads");
            results_left.extend(results_right);
            results_left
        })
        .unwrap();

    results
}

pub trait HashJoin<T> {
    fn hash_join_inner(&self, other: &ChunkedArray<T>) -> Vec<(usize, usize)>;
    fn hash_join_left(&self, other: &ChunkedArray<T>) -> Vec<(usize, Option<usize>)>;
    fn hash_join_outer(
        &self,
        other: &ChunkedArray<T>,
    ) -> HashSet<(Option<usize>, Option<usize>), FnvBuildHasher>;
}

macro_rules! create_join_tuples {
    ($self:expr, $other:expr) => {{
        // The shortest relation will be used to create a hash table.
        let left_first = $self.len() > $other.len();
        let a;
        let b;
        if left_first {
            a = $self;
            b = $other;
        } else {
            b = $self;
            a = $other;
        }

        (a, b, !left_first)
    }};
}

impl<T> HashJoin<T> for ChunkedArray<T>
where
    T: PolarsNumericType + Sync,
    T::Native: Eq + Hash,
{
    fn hash_join_inner(&self, other: &ChunkedArray<T>) -> Vec<(usize, usize)> {
        let (a, b, swap) = create_join_tuples!(self, other);

        match (a.cont_slice(), b.cont_slice()) {
            (Ok(a_slice), Ok(b_slice)) => {
                hash_join_tuples_inner(a_slice.iter(), b_slice.iter(), swap)
            }
            (Ok(a_slice), Err(_)) => {
                hash_join_tuples_inner(
                    a_slice.iter().map(|v| Some(*v)), // take ownership
                    b.into_iter(),
                    swap,
                )
            }
            (Err(_), Ok(b_slice)) => {
                hash_join_tuples_inner(a.into_iter(), b_slice.iter().map(|v| Some(*v)), swap)
            }
            (Err(_), Err(_)) => hash_join_tuples_inner(a.into_iter(), b.into_iter(), swap),
        }
    }

    fn hash_join_left(&self, other: &ChunkedArray<T>) -> Vec<(usize, Option<usize>)> {
        match (self.cont_slice(), other.cont_slice()) {
            (Ok(a_slice), Ok(b_slice)) => hash_join_tuples_left(a_slice.iter(), b_slice.iter()),
            (Ok(a_slice), Err(_)) => {
                hash_join_tuples_left(
                    a_slice.iter().map(|v| Some(*v)), // take ownership
                    other.into_iter(),
                )
            }
            (Err(_), Ok(b_slice)) => {
                hash_join_tuples_left(self.into_iter(), b_slice.iter().map(|v| Some(*v)))
            }
            (Err(_), Err(_)) => hash_join_tuples_left(self.into_iter(), other.into_iter()),
        }
    }

    fn hash_join_outer(
        &self,
        other: &ChunkedArray<T>,
    ) -> HashSet<(Option<usize>, Option<usize>), FnvBuildHasher> {
        match (self.cont_slice(), other.cont_slice()) {
            (Ok(a_slice), Ok(b_slice)) => hash_join_tuples_outer(
                || Box::new(a_slice.iter()),
                || Box::new(b_slice.iter()),
                self.len() + other.len(),
            ),
            (Ok(a_slice), Err(_)) => {
                hash_join_tuples_outer(
                    || Box::new(a_slice.iter().map(|v| Some(*v))), // take ownership
                    || Box::new(other.into_iter()),
                    self.len() + other.len(),
                )
            }
            (Err(_), Ok(b_slice)) => hash_join_tuples_outer(
                || Box::new(self.into_iter()),
                || Box::new(b_slice.iter().map(|v: &T::Native| Some(*v))),
                self.len() + other.len(),
            ),
            (Err(_), Err(_)) => hash_join_tuples_outer(
                || Box::new(self.into_iter()),
                || Box::new(other.into_iter()),
                self.len() + other.len(),
            ),
        }
    }
}

impl HashJoin<BooleanType> for BooleanChunked {
    fn hash_join_inner(&self, other: &BooleanChunked) -> Vec<(usize, usize)> {
        let (a, b, swap) = create_join_tuples!(self, other);
        // Create the join tuples
        hash_join_tuples_inner(a.into_iter(), b.into_iter(), swap)
    }

    fn hash_join_left(&self, other: &BooleanChunked) -> Vec<(usize, Option<usize>)> {
        hash_join_tuples_left(self.into_iter(), other.into_iter())
    }

    fn hash_join_outer(
        &self,
        other: &BooleanChunked,
    ) -> HashSet<(Option<usize>, Option<usize>), FnvBuildHasher> {
        hash_join_tuples_outer(
            || Box::new(self.into_iter()),
            || Box::new(other.into_iter()),
            self.len() + other.len(),
        )
    }
}

impl HashJoin<Utf8Type> for Utf8Chunked {
    fn hash_join_inner(&self, other: &Utf8Chunked) -> Vec<(usize, usize)> {
        let (a, b, swap) = create_join_tuples!(self, other);
        // Create the join tuples
        hash_join_tuples_inner(a.into_iter(), b.into_iter(), swap)
    }

    fn hash_join_left(&self, other: &Utf8Chunked) -> Vec<(usize, Option<usize>)> {
        hash_join_tuples_left(self.into_iter(), other.into_iter())
    }

    fn hash_join_outer(
        &self,
        other: &Utf8Chunked,
    ) -> HashSet<(Option<usize>, Option<usize>), FnvBuildHasher> {
        hash_join_tuples_outer(
            || Box::new(self.into_iter()),
            || Box::new(other.into_iter()),
            self.len() + other.len(),
        )
    }
}

macro_rules! prep_left_and_right_concurrent {
    ($self:ident, $other:ident, $join_tuples:ident, $closure:expr) => {{
        thread::scope(|s| {
            let handle_left =
                s.spawn(|_| $self.create_left_df(&$join_tuples).expect("could not take"));
            let handle_right = s.spawn(|_| {
                let df_right = $other
                    .take_iter($join_tuples.iter().map($closure), Some($join_tuples.len()))
                    .expect("could not take");
                df_right
            });
            let df_left = handle_left.join().expect("could not joint threads");
            let df_right = handle_right.join().expect("could not join threads");
            (df_left, df_right)
        })
        .expect("could not join threads")
    }};
}

impl DataFrame {
    /// Utility method to finish a join.
    fn finish_join(
        &self,
        mut df_left: DataFrame,
        mut df_right: DataFrame,
        right_on: &str,
    ) -> Result<DataFrame> {
        df_right.drop(right_on)?;
        let mut left_names =
            HashSet::with_capacity_and_hasher(df_left.width(), FnvBuildHasher::default());
        for field in df_left.schema.fields() {
            left_names.insert(field.name());
        }

        let mut rename_strs = Vec::with_capacity(df_right.width());

        for field in df_right.schema.fields() {
            if left_names.contains(field.name()) {
                rename_strs.push(field.name().to_owned())
            }
        }

        for name in rename_strs {
            df_right.rename(&name, &format!("{}_right", name))?
        }

        df_left.hstack(&df_right.columns)?;
        Ok(df_left)
    }

    fn create_left_df<B: Sync>(&self, join_tuples: &[(usize, B)]) -> Result<DataFrame> {
        self.take_iter(
            join_tuples.iter().map(|(left, _right)| Some(*left)),
            Some(join_tuples.len()),
        )
    }

    /// Perform an inner join on two DataFrames.
    ///
    /// # Example
    ///
    /// ```
    /// use polars::prelude::*;
    /// fn join_dfs(left: &DataFrame, right: &DataFrame) -> Result<DataFrame> {
    ///     left.inner_join(right, "join_column_left", "join_column_right")
    /// }
    /// ```
    pub fn inner_join(
        &self,
        other: &DataFrame,
        left_on: &str,
        right_on: &str,
    ) -> Result<DataFrame> {
        let s_left = self.column(left_on).ok_or(PolarsError::NotFound)?;
        let s_right = other.column(right_on).ok_or(PolarsError::NotFound)?;
        let join_tuples = apply_hash_join_on_series!(s_left, s_right, hash_join_inner);
        let (df_left, df_right) =
            prep_left_and_right_concurrent!(self, other, join_tuples, |(_left, right)| Some(
                *right
            ));
        self.finish_join(df_left, df_right, right_on)
    }

    /// Perform a left join on two DataFrames
    /// # Example
    ///
    /// ```
    /// use polars::prelude::*;
    /// fn join_dfs(left: &DataFrame, right: &DataFrame) -> Result<DataFrame> {
    ///     left.left_join(right, "join_column_left", "join_column_right")
    /// }
    /// ```
    pub fn left_join(&self, other: &DataFrame, left_on: &str, right_on: &str) -> Result<DataFrame> {
        let s_left = self.column(left_on).ok_or(PolarsError::NotFound)?;
        let s_right = other.column(right_on).ok_or(PolarsError::NotFound)?;
        let opt_join_tuples: Vec<(usize, Option<usize>)> =
            apply_hash_join_on_series!(s_left, s_right, hash_join_left);
        let (df_left, df_right) =
            prep_left_and_right_concurrent!(self, other, opt_join_tuples, |(_left, right)| *right);
        self.finish_join(df_left, df_right, right_on)
    }

    /// Perform an outer join on two DataFrames
    /// # Example
    ///
    /// ```
    /// use polars::prelude::*;
    /// fn join_dfs(left: &DataFrame, right: &DataFrame) -> Result<DataFrame> {
    ///     left.outer_join(right, "join_column_left", "join_column_right")
    /// }
    /// ```
    pub fn outer_join(
        &self,
        other: &DataFrame,
        left_on: &str,
        right_on: &str,
    ) -> Result<DataFrame> {
        let s_left = self.column(left_on).ok_or(PolarsError::NotFound)?;
        let s_right = other.column(right_on).ok_or(PolarsError::NotFound)?;

        let opt_join_tuples: HashSet<(Option<usize>, Option<usize>), FnvBuildHasher> =
            apply_hash_join_on_series!(s_left, s_right, hash_join_outer);

        let (mut df_left, df_right) = thread::scope(|s| {
            let handle_left = s.spawn(|_| {
                let df_left = self
                    .take_iter(
                        opt_join_tuples.iter().map(|(left, _right)| *left),
                        Some(opt_join_tuples.len()),
                    )
                    .expect("could not take");
                df_left
            });

            let handle_right = s.spawn(|_| {
                let df_right = other
                    .take_iter(
                        opt_join_tuples.iter().map(|(_left, right)| *right),
                        Some(opt_join_tuples.len()),
                    )
                    .expect("could not take");
                df_right
            });

            let df_left = handle_left.join().expect("could not joint threads");
            let df_right = handle_right.join().expect("could not join threads");
            (df_left, df_right)
        })
        .expect("could not join threads");

        let left_join_col = df_left.column(left_on).unwrap();
        let right_join_col = df_right.column(right_on).unwrap();

        macro_rules! downcast_and_replace_joined_column {
            ($type:ident) => {{
                let mut join_col: Series = left_join_col
                    .$type()
                    .unwrap()
                    .into_iter()
                    .zip(right_join_col.$type().unwrap().into_iter())
                    .map(|(left, right)| if left.is_some() { left } else { right })
                    .collect();
                join_col.rename(left_on);
                df_left.replace(left_on, join_col)?;
            }};
        }

        if left_join_col.null_count() > 0 {
            match s_left.dtype() {
                ArrowDataType::UInt32 => downcast_and_replace_joined_column!(u32),
                ArrowDataType::Int32 => downcast_and_replace_joined_column!(i32),
                ArrowDataType::Int64 => downcast_and_replace_joined_column!(i64),
                ArrowDataType::Date32(DateUnit::Millisecond) => {
                    downcast_and_replace_joined_column!(i32)
                }
                ArrowDataType::Date64(DateUnit::Millisecond) => {
                    downcast_and_replace_joined_column!(i64)
                }
                ArrowDataType::Duration(TimeUnit::Nanosecond) => {
                    downcast_and_replace_joined_column!(i64)
                }
                ArrowDataType::Time64(TimeUnit::Nanosecond) => {
                    downcast_and_replace_joined_column!(i64)
                }
                ArrowDataType::Boolean => downcast_and_replace_joined_column!(bool),
                ArrowDataType::Utf8 => {
                    // string has no nulls but empty strings,
                    let mut join_col: Series = left_join_col
                        .utf8()
                        .unwrap()
                        .into_iter()
                        .zip(right_join_col.utf8().unwrap().into_iter())
                        .map(|(left, right)| if left.len() == 0 { left } else { right })
                        .collect();
                    join_col.rename(left_on);
                    df_left.replace(left_on, join_col)?;
                }
                _ => unimplemented!(),
            }
        }
        self.finish_join(df_left, df_right, right_on)
    }
}

#[cfg(test)]
mod test {
    use crate::prelude::*;

    fn create_frames() -> (DataFrame, DataFrame) {
        let s0 = Series::new("days", [0, 1, 2].as_ref());
        let s1 = Series::new("temp", [22.1, 19.9, 7.].as_ref());
        let s2 = Series::new("rain", [0.2, 0.1, 0.3].as_ref());
        let temp = DataFrame::new(vec![s0, s1, s2]).unwrap();

        let s0 = Series::new("days", [1, 2, 3, 1].as_ref());
        let s1 = Series::new("rain", [0.1, 0.2, 0.3, 0.4].as_ref());
        let rain = DataFrame::new(vec![s0, s1]).unwrap();
        (temp, rain)
    }

    #[test]
    fn test_inner_join() {
        let (temp, rain) = create_frames();
        let joined = temp.inner_join(&rain, "days", "days").unwrap();

        let join_col_days = Series::new("days", [1, 2, 1].as_ref());
        let join_col_temp = Series::new("temp", [19.9, 7., 19.9].as_ref());
        let join_col_rain = Series::new("rain", [0.1, 0.3, 0.1].as_ref());
        let join_col_rain_right = Series::new("rain_right", [0.1, 0.2, 0.4].as_ref());
        let true_df = DataFrame::new(vec![
            join_col_days,
            join_col_temp,
            join_col_rain,
            join_col_rain_right,
        ])
        .unwrap();

        assert!(joined.frame_equal(&true_df));
        println!("{}", joined)
    }

    #[test]
    fn test_left_join() {
        let s0 = Series::new("days", [0, 1, 2, 3, 4].as_ref());
        let s1 = Series::new("temp", [22.1, 19.9, 7., 2., 3.].as_ref());
        let temp = DataFrame::new(vec![s0, s1]).unwrap();

        let s0 = Series::new("days", [1, 2].as_ref());
        let s1 = Series::new("rain", [0.1, 0.2].as_ref());
        let rain = DataFrame::new(vec![s0, s1]).unwrap();
        let joined = temp.left_join(&rain, "days", "days").unwrap();
        println!("{}", &joined);
        assert_eq!(
            (joined.f_column("rain").sum::<f32>().unwrap() * 10.).round(),
            3.
        );
        assert_eq!(joined.f_column("rain").null_count(), 3)
    }

    #[test]
    fn test_outer_join() {
        let (temp, rain) = create_frames();
        let joined = temp.outer_join(&rain, "days", "days").unwrap();
        assert_eq!(joined.height(), 5);
        assert_eq!(joined.column("days").unwrap().sum::<i32>(), Some(7));
        println!("{:?}", &joined);
    }
}
