//! This module defines R1CS related types and a folding scheme for Relaxed R1CS
use crate::{
  constants::{BN_LIMB_WIDTH, BN_N_LIMBS},
  digest::{DigestComputer, SimpleDigestible},
  errors::NovaError,
  gadgets::{
    nonnative::{bignat::nat_to_limbs, util::f_to_nat},
    utils::scalar_as_base,
  },
  traits::{
    commitment::CommitmentEngineTrait, AbsorbInRO2Trait, AbsorbInROTrait, Engine, ROTrait,
    TranscriptReprTrait,
  },
  Commitment, CommitmentKey, DerandKey, CE,
};
use core::cmp::max;
use ff::Field;
use once_cell::sync::OnceCell;
use rand_core::OsRng;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

mod sparse;
pub(crate) use sparse::SparseMatrix;

/// A type that holds the shape of the R1CS matrices
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R1CSShape<E: Engine> {
  pub(crate) num_cons: usize,
  pub(crate) num_vars: usize,
  pub(crate) num_io: usize,
  pub(crate) A: SparseMatrix<E::Scalar>,
  pub(crate) B: SparseMatrix<E::Scalar>,
  pub(crate) C: SparseMatrix<E::Scalar>,
  #[serde(skip, default = "OnceCell::new")]
  pub(crate) digest: OnceCell<E::Scalar>,
}

impl<E: Engine> SimpleDigestible for R1CSShape<E> {}

/// A type that holds a witness for a given R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R1CSWitness<E: Engine> {
  pub(crate) W: Vec<E::Scalar>,
  pub(crate) r_W: E::Scalar,
}

/// A type that holds an R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct R1CSInstance<E: Engine> {
  pub(crate) comm_W: Commitment<E>,
  pub(crate) X: Vec<E::Scalar>,
}

/// A type that holds a witness for a given Relaxed R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaxedR1CSWitness<E: Engine> {
  pub(crate) W: Vec<E::Scalar>,
  pub(crate) r_W: E::Scalar,
  pub(crate) E: Vec<E::Scalar>,
  pub(crate) r_E: E::Scalar,
}

/// A type that holds a Relaxed R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct RelaxedR1CSInstance<E: Engine> {
  pub(crate) comm_W: Commitment<E>,
  pub(crate) comm_E: Commitment<E>,
  pub(crate) X: Vec<E::Scalar>,
  pub(crate) u: E::Scalar,
}

pub type CommitmentKeyHint<E> = dyn Fn(&R1CSShape<E>) -> usize;

impl<E: Engine> R1CSShape<E> {
  /// Create an object of type `R1CSShape` from the explicitly specified R1CS matrices
  pub fn new(
    num_cons: usize,
    num_vars: usize,
    num_io: usize,
    A: SparseMatrix<E::Scalar>,
    B: SparseMatrix<E::Scalar>,
    C: SparseMatrix<E::Scalar>,
  ) -> Result<R1CSShape<E>, NovaError> {
    let is_valid = |num_cons: usize,
                    num_vars: usize,
                    num_io: usize,
                    M: &SparseMatrix<E::Scalar>|
     -> Result<Vec<()>, NovaError> {
      M.iter()
        .map(|(row, col, _val)| {
          if row >= num_cons || col > num_io + num_vars {
            Err(NovaError::InvalidIndex)
          } else {
            Ok(())
          }
        })
        .collect::<Result<Vec<()>, NovaError>>()
    };

    is_valid(num_cons, num_vars, num_io, &A)?;
    is_valid(num_cons, num_vars, num_io, &B)?;
    is_valid(num_cons, num_vars, num_io, &C)?;

    Ok(R1CSShape {
      num_cons,
      num_vars,
      num_io,
      A,
      B,
      C,
      digest: OnceCell::new(),
    })
  }

  /// Generates public parameters for a Rank-1 Constraint System (R1CS).
  ///
  /// This function takes into consideration the shape of the R1CS matrices and a hint function
  /// for the number of generators. It returns a `CommitmentKey`.
  ///
  /// # Arguments
  ///
  /// * `S`: The shape of the R1CS matrices.
  /// * `ck_floor`: A function that provides a floor for the number of generators. A good function
  ///   to provide is the ck_floor field defined in the trait `RelaxedR1CSSNARKTrait`.
  ///
  pub fn commitment_key(&self, ck_floor: &CommitmentKeyHint<E>) -> CommitmentKey<E> {
    let num_cons = self.num_cons;
    let num_vars = self.num_vars;
    let ck_hint = ck_floor(self);
    E::CE::setup(b"ck", max(max(num_cons, num_vars), ck_hint))
  }

  /// Returns the digest of the `R1CSShape`
  pub fn digest(&self) -> E::Scalar {
    self
      .digest
      .get_or_try_init(|| DigestComputer::new(self).digest())
      .cloned()
      .expect("Failure in retrieving digest")
  }

  // Checks regularity conditions on the R1CSShape, required in Spartan-class SNARKs
  // Returns false if num_cons or num_vars are not powers of two, or if num_io > num_vars
  #[inline]
  pub(crate) fn is_regular_shape(&self) -> bool {
    let cons_valid = self.num_cons.next_power_of_two() == self.num_cons;
    let vars_valid = self.num_vars.next_power_of_two() == self.num_vars;
    let io_lt_vars = self.num_io < self.num_vars;
    cons_valid && vars_valid && io_lt_vars
  }

  pub fn multiply_vec(
    &self,
    z: &[E::Scalar],
  ) -> Result<(Vec<E::Scalar>, Vec<E::Scalar>, Vec<E::Scalar>), NovaError> {
    if z.len() != self.num_io + self.num_vars + 1 {
      return Err(NovaError::InvalidWitnessLength);
    }

    let (Az, (Bz, Cz)) = rayon::join(
      || self.A.multiply_vec(z),
      || rayon::join(|| self.B.multiply_vec(z), || self.C.multiply_vec(z)),
    );

    Ok((Az, Bz, Cz))
  }

  /// Checks if the Relaxed R1CS instance is satisfiable given a witness and its shape
  pub fn is_sat_relaxed(
    &self,
    ck: &CommitmentKey<E>,
    U: &RelaxedR1CSInstance<E>,
    W: &RelaxedR1CSWitness<E>,
  ) -> Result<(), NovaError> {
    assert_eq!(W.W.len(), self.num_vars);
    assert_eq!(W.E.len(), self.num_cons);
    assert_eq!(U.X.len(), self.num_io);

    // verify if Az * Bz = u*Cz + E
    let res_eq = {
      let z = [W.W.clone(), vec![U.u], U.X.clone()].concat();
      let (Az, Bz, Cz) = self.multiply_vec(&z)?;
      assert_eq!(Az.len(), self.num_cons);
      assert_eq!(Bz.len(), self.num_cons);
      assert_eq!(Cz.len(), self.num_cons);

      (0..self.num_cons).all(|i| Az[i] * Bz[i] == U.u * Cz[i] + W.E[i])
    };

    // verify if comm_E and comm_W are commitments to E and W
    let res_comm = {
      let (comm_W, comm_E) = rayon::join(
        || CE::<E>::commit(ck, &W.W, &W.r_W),
        || CE::<E>::commit(ck, &W.E, &W.r_E),
      );
      U.comm_W == comm_W && U.comm_E == comm_E
    };

    if !res_eq {
      return Err(NovaError::UnSat {
        reason: "Relaxed R1CS is unsatisfiable".to_string(),
      });
    }

    if !res_comm {
      return Err(NovaError::UnSat {
        reason: "Invalid commitments".to_string(),
      });
    }

    Ok(())
  }

  /// Checks if the R1CS instance is satisfiable given a witness and its shape
  pub fn is_sat(
    &self,
    ck: &CommitmentKey<E>,
    U: &R1CSInstance<E>,
    W: &R1CSWitness<E>,
  ) -> Result<(), NovaError> {
    assert_eq!(W.W.len(), self.num_vars);
    assert_eq!(U.X.len(), self.num_io);

    // verify if Az * Bz = u*Cz
    let res_eq = {
      let z = [W.W.clone(), vec![E::Scalar::ONE], U.X.clone()].concat();
      let (Az, Bz, Cz) = self.multiply_vec(&z)?;
      assert_eq!(Az.len(), self.num_cons);
      assert_eq!(Bz.len(), self.num_cons);
      assert_eq!(Cz.len(), self.num_cons);

      (0..self.num_cons).all(|i| Az[i] * Bz[i] == Cz[i])
    };

    // verify if comm_W is a commitment to W
    let res_comm = U.comm_W == CE::<E>::commit(ck, &W.W, &W.r_W);

    if !res_eq {
      return Err(NovaError::UnSat {
        reason: "R1CS is unsatisfiable".to_string(),
      });
    }

    if !res_comm {
      return Err(NovaError::UnSat {
        reason: "Invalid commitment".to_string(),
      });
    }

    Ok(())
  }

  /// A method to compute a commitment to the cross-term `T` given a
  /// Relaxed R1CS instance-witness pair and an R1CS instance-witness pair
  pub fn commit_T(
    &self,
    ck: &CommitmentKey<E>,
    U1: &RelaxedR1CSInstance<E>,
    W1: &RelaxedR1CSWitness<E>,
    U2: &R1CSInstance<E>,
    W2: &R1CSWitness<E>,
    r_T: &E::Scalar,
  ) -> Result<(Vec<E::Scalar>, Commitment<E>), NovaError> {
    let Z1 = [W1.W.clone(), vec![U1.u], U1.X.clone()].concat();
    let Z2 = [W2.W.clone(), vec![E::Scalar::ONE], U2.X.clone()].concat();

    // The following code uses the optimization suggested in
    // Section 5.2 of [Mova](https://eprint.iacr.org/2024/1220.pdf)
    let Z = Z1
      .into_par_iter()
      .zip(Z2.into_par_iter())
      .map(|(z1, z2)| z1 + z2)
      .collect::<Vec<E::Scalar>>();
    let u = U1.u + E::Scalar::ONE; // U2.u = 1

    let (AZ, BZ, CZ) = self.multiply_vec(&Z)?;

    let T = AZ
      .par_iter()
      .zip(BZ.par_iter())
      .zip(CZ.par_iter())
      .zip(W1.E.par_iter())
      .map(|(((az, bz), cz), e)| *az * *bz - u * *cz - *e)
      .collect::<Vec<E::Scalar>>();

    let comm_T = CE::<E>::commit(ck, &T, r_T);

    Ok((T, comm_T))
  }

  /// A method to compute a commitment to the cross-term `T` given two
  /// Relaxed R1CS instance-witness pairs
  pub fn commit_T_relaxed(
    &self,
    ck: &CommitmentKey<E>,
    U1: &RelaxedR1CSInstance<E>,
    W1: &RelaxedR1CSWitness<E>,
    U2: &RelaxedR1CSInstance<E>,
    W2: &RelaxedR1CSWitness<E>,
    r_T: &E::Scalar,
  ) -> Result<(Vec<E::Scalar>, Commitment<E>), NovaError> {
    let Z1 = [W1.W.clone(), vec![U1.u], U1.X.clone()].concat();
    let Z2 = [W2.W.clone(), vec![U2.u], U2.X.clone()].concat();

    // The following code uses the optimization suggested in
    // Section 5.2 of [Mova](https://eprint.iacr.org/2024/1220.pdf)
    let Z = Z1
      .into_par_iter()
      .zip(Z2.into_par_iter())
      .map(|(z1, z2)| z1 + z2)
      .collect::<Vec<E::Scalar>>();
    let u = U1.u + U2.u;

    let (AZ, BZ, CZ) = self.multiply_vec(&Z)?;

    let T = AZ
      .par_iter()
      .zip(BZ.par_iter())
      .zip(CZ.par_iter())
      .zip(W1.E.par_iter())
      .zip(W2.E.par_iter())
      .map(|((((az, bz), cz), e1), e2)| *az * *bz - u * *cz - *e1 - *e2)
      .collect::<Vec<E::Scalar>>();

    let comm_T = CE::<E>::commit(ck, &T, r_T);

    Ok((T, comm_T))
  }

  /// Pads the `R1CSShape` so that the shape passes `is_regular_shape`
  /// Renumbers variables to accommodate padded variables
  pub fn pad(&self) -> Self {
    // check if the provided R1CSShape is already as required
    if self.is_regular_shape() {
      return self.clone();
    }

    // equalize the number of variables, constraints, and public IO
    let m = max(max(self.num_vars, self.num_cons), self.num_io).next_power_of_two();

    // check if the number of variables are as expected, then
    // we simply set the number of constraints to the next power of two
    if self.num_vars == m {
      return R1CSShape {
        num_cons: m,
        num_vars: m,
        num_io: self.num_io,
        A: self.A.clone(),
        B: self.B.clone(),
        C: self.C.clone(),
        digest: OnceCell::new(),
      };
    }

    // otherwise, we need to pad the number of variables and renumber variable accesses
    let num_vars_padded = m;
    let num_cons_padded = m;

    let apply_pad = |mut M: SparseMatrix<E::Scalar>| -> SparseMatrix<E::Scalar> {
      M.indices.par_iter_mut().for_each(|c| {
        if *c >= self.num_vars {
          *c += num_vars_padded - self.num_vars
        }
      });

      M.cols += num_vars_padded - self.num_vars;

      let ex = {
        let nnz = M.indptr.last().unwrap();
        vec![*nnz; num_cons_padded - self.num_cons]
      };
      M.indptr.extend(ex);
      M
    };

    let A_padded = apply_pad(self.A.clone());
    let B_padded = apply_pad(self.B.clone());
    let C_padded = apply_pad(self.C.clone());

    R1CSShape {
      num_cons: num_cons_padded,
      num_vars: num_vars_padded,
      num_io: self.num_io,
      A: A_padded,
      B: B_padded,
      C: C_padded,
      digest: OnceCell::new(),
    }
  }

  /// Samples a new random `RelaxedR1CSInstance`/`RelaxedR1CSWitness` pair
  pub fn sample_random_instance_witness(
    &self,
    ck: &CommitmentKey<E>,
  ) -> Result<(RelaxedR1CSInstance<E>, RelaxedR1CSWitness<E>), NovaError> {
    // sample Z = (W, u, X)
    let Z = (0..self.num_vars + self.num_io + 1)
      .into_par_iter()
      .map(|_| E::Scalar::random(&mut OsRng))
      .collect::<Vec<E::Scalar>>();

    let r_W = E::Scalar::random(&mut OsRng);
    let r_E = E::Scalar::random(&mut OsRng);

    let u = Z[self.num_vars];

    // compute E <- AZ o BZ - u * CZ
    let (AZ, BZ, CZ) = self.multiply_vec(&Z)?;

    let E = AZ
      .par_iter()
      .zip(BZ.par_iter())
      .zip(CZ.par_iter())
      .map(|((az, bz), cz)| *az * *bz - u * *cz)
      .collect::<Vec<E::Scalar>>();

    // compute commitments to W,E in parallel
    let (comm_W, comm_E) = rayon::join(
      || CE::<E>::commit(ck, &Z[..self.num_vars], &r_W),
      || CE::<E>::commit(ck, &E, &r_E),
    );

    Ok((
      RelaxedR1CSInstance {
        comm_W,
        comm_E,
        u,
        X: Z[self.num_vars + 1..].to_vec(),
      },
      RelaxedR1CSWitness {
        W: Z[..self.num_vars].to_vec(),
        r_W,
        E,
        r_E,
      },
    ))
  }
}

impl<E: Engine> R1CSWitness<E> {
  /// A method to create a witness object using a vector of scalars
  pub fn new(S: &R1CSShape<E>, W: &[E::Scalar]) -> Result<R1CSWitness<E>, NovaError> {
    let mut W = W.to_vec();
    W.resize(S.num_vars, E::Scalar::ZERO);

    Ok(R1CSWitness {
      W,
      r_W: E::Scalar::random(&mut OsRng),
    })
  }

  /// Commits to the witness using the supplied generators
  pub fn commit(&self, ck: &CommitmentKey<E>) -> Commitment<E> {
    CE::<E>::commit(ck, &self.W, &self.r_W)
  }

  /// Pads the provided witness to the correct length
  pub fn pad(&self, S: &R1CSShape<E>) -> R1CSWitness<E> {
    let mut W = self.W.clone();
    W.extend(vec![E::Scalar::ZERO; S.num_vars - W.len()]);

    Self { W, r_W: self.r_W }
  }
}

impl<E: Engine> R1CSInstance<E> {
  /// A method to create an instance object using constituent elements
  pub fn new(
    S: &R1CSShape<E>,
    comm_W: &Commitment<E>,
    X: &[E::Scalar],
  ) -> Result<R1CSInstance<E>, NovaError> {
    if S.num_io != X.len() {
      Err(NovaError::InvalidInputLength)
    } else {
      Ok(R1CSInstance {
        comm_W: *comm_W,
        X: X.to_owned(),
      })
    }
  }
}

impl<E: Engine> AbsorbInROTrait<E> for R1CSInstance<E> {
  fn absorb_in_ro(&self, ro: &mut E::RO) {
    self.comm_W.absorb_in_ro(ro);

    // In Nova's folding scheme, the public IO of the R1CS instance only contains hashes
    // These hashes have unique representations in the base field
    for x in &self.X {
      ro.absorb(scalar_as_base::<E>(*x));
    }
  }
}

impl<E: Engine> AbsorbInRO2Trait<E> for R1CSInstance<E> {
  fn absorb_in_ro2(&self, ro: &mut E::RO2) {
    // we have to absorb the commitment to W in RO2
    self.comm_W.absorb_in_ro2(ro);

    for x in &self.X {
      ro.absorb(*x);
    }
  }
}

impl<E: Engine> RelaxedR1CSWitness<E> {
  /// Produces a default `RelaxedR1CSWitness` given an `R1CSShape`
  pub fn default(S: &R1CSShape<E>) -> RelaxedR1CSWitness<E> {
    RelaxedR1CSWitness {
      W: vec![E::Scalar::ZERO; S.num_vars],
      r_W: E::Scalar::ZERO,
      E: vec![E::Scalar::ZERO; S.num_cons],
      r_E: E::Scalar::ZERO,
    }
  }

  /// Initializes a new `RelaxedR1CSWitness` from an `R1CSWitness`
  pub fn from_r1cs_witness(S: &R1CSShape<E>, witness: &R1CSWitness<E>) -> RelaxedR1CSWitness<E> {
    RelaxedR1CSWitness {
      W: witness.W.clone(),
      r_W: witness.r_W,
      E: vec![E::Scalar::ZERO; S.num_cons],
      r_E: E::Scalar::ZERO,
    }
  }

  /// Commits to the witness using the supplied generators
  pub fn commit(&self, ck: &CommitmentKey<E>) -> (Commitment<E>, Commitment<E>) {
    (
      CE::<E>::commit(ck, &self.W, &self.r_W),
      CE::<E>::commit(ck, &self.E, &self.r_E),
    )
  }

  /// Folds an incoming `R1CSWitness` into the current one
  pub fn fold(
    &self,
    W2: &R1CSWitness<E>,
    T: &[E::Scalar],
    r_T: &E::Scalar,
    r: &E::Scalar,
  ) -> Result<RelaxedR1CSWitness<E>, NovaError> {
    let (W1, r_W1, E1, r_E1) = (&self.W, &self.r_W, &self.E, &self.r_E);
    let (W2, r_W2) = (&W2.W, &W2.r_W);

    if W1.len() != W2.len() {
      return Err(NovaError::InvalidWitnessLength);
    }

    let W = W1
      .par_iter()
      .zip(W2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<E::Scalar>>();
    let E = E1
      .par_iter()
      .zip(T)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<E::Scalar>>();

    let r_W = *r_W1 + *r * r_W2;
    let r_E = *r_E1 + *r * r_T;

    Ok(RelaxedR1CSWitness { W, r_W, E, r_E })
  }

  /// Folds an incoming `RelaxedR1CSWitness` into the current one
  /// E2 is not necessarily zero vec  
  pub fn fold_relaxed(
    &self,
    W2: &RelaxedR1CSWitness<E>,
    T: &[E::Scalar],
    r_T: &E::Scalar,
    r: &E::Scalar,
  ) -> Result<RelaxedR1CSWitness<E>, NovaError> {
    let (W1, r_W1, E1, r_E1) = (&self.W, &self.r_W, &self.E, &self.r_E);
    let (W2, r_W2, E2, r_E2) = (&W2.W, &W2.r_W, &W2.E, &W2.r_E);

    if W1.len() != W2.len() {
      return Err(NovaError::InvalidWitnessLength);
    }

    let W = W1
      .par_iter()
      .zip(W2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<E::Scalar>>();
    let E = E1
      .par_iter()
      .zip(T)
      .zip(E2.par_iter())
      .map(|((a, b), c)| *a + *r * *b + *r * *r * *c)
      .collect::<Vec<E::Scalar>>();

    let r_W = *r_W1 + *r * r_W2;
    let r_E = *r_E1 + *r * r_T + *r * *r * *r_E2;

    Ok(RelaxedR1CSWitness { W, r_W, E, r_E })
  }

  /// Pads the provided witness to the correct length
  pub fn pad(&self, S: &R1CSShape<E>) -> RelaxedR1CSWitness<E> {
    let mut W = self.W.clone();
    W.extend(vec![E::Scalar::ZERO; S.num_vars - W.len()]);

    let mut E = self.E.clone();
    E.extend(vec![E::Scalar::ZERO; S.num_cons - E.len()]);

    Self {
      W,
      r_W: self.r_W,
      E,
      r_E: self.r_E,
    }
  }

  pub fn derandomize(&self) -> (Self, E::Scalar, E::Scalar) {
    (
      RelaxedR1CSWitness {
        W: self.W.clone(),
        r_W: E::Scalar::ZERO,
        E: self.E.clone(),
        r_E: E::Scalar::ZERO,
      },
      self.r_W,
      self.r_E,
    )
  }
}

impl<E: Engine> RelaxedR1CSInstance<E> {
  /// Produces a default `RelaxedR1CSInstance` given `R1CSGens` and `R1CSShape`
  pub fn default(_ck: &CommitmentKey<E>, S: &R1CSShape<E>) -> RelaxedR1CSInstance<E> {
    let (comm_W, comm_E) = (Commitment::<E>::default(), Commitment::<E>::default());
    RelaxedR1CSInstance {
      comm_W,
      comm_E,
      u: E::Scalar::ZERO,
      X: vec![E::Scalar::ZERO; S.num_io],
    }
  }

  /// Initializes a new `RelaxedR1CSInstance` from an `R1CSInstance`
  pub fn from_r1cs_instance(
    ck: &CommitmentKey<E>,
    S: &R1CSShape<E>,
    instance: &R1CSInstance<E>,
  ) -> RelaxedR1CSInstance<E> {
    let mut r_instance = RelaxedR1CSInstance::default(ck, S);
    r_instance.comm_W = instance.comm_W;
    r_instance.u = E::Scalar::ONE;
    r_instance.X.clone_from(&instance.X);

    r_instance
  }

  /// Initializes a new `RelaxedR1CSInstance` from an `R1CSInstance`
  pub fn from_r1cs_instance_unchecked(
    comm_W: &Commitment<E>,
    X: &[E::Scalar],
  ) -> RelaxedR1CSInstance<E> {
    RelaxedR1CSInstance {
      comm_W: *comm_W,
      comm_E: Commitment::<E>::default(),
      u: E::Scalar::ONE,
      X: X.to_vec(),
    }
  }

  /// Folds an incoming `R1CSInstance` into the current one
  pub fn fold(
    &self,
    U2: &R1CSInstance<E>,
    comm_T: &Commitment<E>,
    r: &E::Scalar,
  ) -> RelaxedR1CSInstance<E> {
    let (X1, u1, comm_W_1, comm_E_1) =
      (&self.X, &self.u, &self.comm_W.clone(), &self.comm_E.clone());
    let (X2, comm_W_2) = (&U2.X, &U2.comm_W);

    // weighted sum of X, comm_W, comm_E, and u
    let X = X1
      .par_iter()
      .zip(X2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<E::Scalar>>();
    let comm_W = *comm_W_1 + *comm_W_2 * *r;
    let comm_E = *comm_E_1 + *comm_T * *r;
    let u = *u1 + *r;

    RelaxedR1CSInstance {
      comm_W,
      comm_E,
      X,
      u,
    }
  }

  /// Folds an incoming `RelaxedR1CSInstance` into the current one
  pub fn fold_relaxed(
    &self,
    U2: &RelaxedR1CSInstance<E>,
    comm_T: &Commitment<E>,
    r: &E::Scalar,
  ) -> RelaxedR1CSInstance<E> {
    let (X1, u1, comm_W_1, comm_E_1) =
      (&self.X, &self.u, &self.comm_W.clone(), &self.comm_E.clone());
    let (X2, u2, comm_W_2, comm_E_2) = (&U2.X, &U2.u, &U2.comm_W, &U2.comm_E);

    // weighted sum of X, comm_W, comm_E, and u
    let X = X1
      .par_iter()
      .zip(X2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<E::Scalar>>();
    let comm_W = *comm_W_1 + *comm_W_2 * *r;
    let comm_E = *comm_E_1 + *comm_T * *r + *comm_E_2 * *r * *r;
    let u = *u1 + *r * *u2;

    RelaxedR1CSInstance {
      comm_W,
      comm_E,
      X,
      u,
    }
  }

  pub fn derandomize(
    &self,
    dk: &DerandKey<E>,
    r_W: &E::Scalar,
    r_E: &E::Scalar,
  ) -> RelaxedR1CSInstance<E> {
    RelaxedR1CSInstance {
      comm_W: CE::<E>::derandomize(dk, &self.comm_W, r_W),
      comm_E: CE::<E>::derandomize(dk, &self.comm_E, r_E),
      X: self.X.clone(),
      u: self.u,
    }
  }
}

impl<E: Engine> TranscriptReprTrait<E::GE> for RelaxedR1CSInstance<E> {
  fn to_transcript_bytes(&self) -> Vec<u8> {
    [
      self.comm_W.to_transcript_bytes(),
      self.comm_E.to_transcript_bytes(),
      self.u.to_transcript_bytes(),
      self.X.as_slice().to_transcript_bytes(),
    ]
    .concat()
  }
}

impl<E: Engine> AbsorbInROTrait<E> for RelaxedR1CSInstance<E> {
  fn absorb_in_ro(&self, ro: &mut E::RO) {
    self.comm_W.absorb_in_ro(ro);
    self.comm_E.absorb_in_ro(ro);
    ro.absorb(scalar_as_base::<E>(self.u));

    // absorb each element of self.X in bignum format
    for x in &self.X {
      let limbs: Vec<E::Scalar> = nat_to_limbs(&f_to_nat(x), BN_LIMB_WIDTH, BN_N_LIMBS).unwrap();
      for limb in limbs {
        ro.absorb(scalar_as_base::<E>(limb));
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use ff::Field;

  use super::*;
  use crate::{
    provider::{Bn256EngineKZG, PallasEngine, Secp256k1Engine},
    r1cs::sparse::SparseMatrix,
    traits::{snark::default_ck_hint, Engine},
  };

  fn tiny_r1cs<E: Engine>(num_vars: usize) -> R1CSShape<E> {
    let one = <E::Scalar as Field>::ONE;
    let (num_cons, num_vars, num_io, A, B, C) = {
      let num_cons = 4;
      let num_io = 2;

      // Consider a cubic equation: `x^3 + x + 5 = y`, where `x` and `y` are respectively the input and output.
      // The R1CS for this problem consists of the following constraints:
      // `I0 * I0 - Z0 = 0`
      // `Z0 * I0 - Z1 = 0`
      // `(Z1 + I0) * 1 - Z2 = 0`
      // `(Z2 + 5) * 1 - I1 = 0`

      // Relaxed R1CS is a set of three sparse matrices (A B C), where there is a row for every
      // constraint and a column for every entry in z = (vars, u, inputs)
      // An R1CS instance is satisfiable iff:
      // Az \circ Bz = u \cdot Cz + E, where z = (vars, 1, inputs)
      let mut A: Vec<(usize, usize, E::Scalar)> = Vec::new();
      let mut B: Vec<(usize, usize, E::Scalar)> = Vec::new();
      let mut C: Vec<(usize, usize, E::Scalar)> = Vec::new();

      // constraint 0 entries in (A,B,C)
      // `I0 * I0 - Z0 = 0`
      A.push((0, num_vars + 1, one));
      B.push((0, num_vars + 1, one));
      C.push((0, 0, one));

      // constraint 1 entries in (A,B,C)
      // `Z0 * I0 - Z1 = 0`
      A.push((1, 0, one));
      B.push((1, num_vars + 1, one));
      C.push((1, 1, one));

      // constraint 2 entries in (A,B,C)
      // `(Z1 + I0) * 1 - Z2 = 0`
      A.push((2, 1, one));
      A.push((2, num_vars + 1, one));
      B.push((2, num_vars, one));
      C.push((2, 2, one));

      // constraint 3 entries in (A,B,C)
      // `(Z2 + 5) * 1 - I1 = 0`
      A.push((3, 2, one));
      A.push((3, num_vars, one + one + one + one + one));
      B.push((3, num_vars, one));
      C.push((3, num_vars + 2, one));

      (num_cons, num_vars, num_io, A, B, C)
    };

    // create a shape object
    let rows = num_cons;
    let cols = num_vars + num_io + 1;

    let res = R1CSShape::new(
      num_cons,
      num_vars,
      num_io,
      SparseMatrix::new(&A, rows, cols),
      SparseMatrix::new(&B, rows, cols),
      SparseMatrix::new(&C, rows, cols),
    );
    assert!(res.is_ok());
    res.unwrap()
  }

  fn test_pad_tiny_r1cs_with<E: Engine>() {
    let padded_r1cs = tiny_r1cs::<E>(3).pad();
    assert!(padded_r1cs.is_regular_shape());

    let expected_r1cs = tiny_r1cs::<E>(4);

    assert_eq!(padded_r1cs, expected_r1cs);
  }

  #[test]
  fn test_pad_tiny_r1cs() {
    test_pad_tiny_r1cs_with::<PallasEngine>();
    test_pad_tiny_r1cs_with::<Bn256EngineKZG>();
    test_pad_tiny_r1cs_with::<Secp256k1Engine>();
  }

  fn test_random_sample_with<E: Engine>() {
    let S = tiny_r1cs::<E>(4);
    let ck = S.commitment_key(&*default_ck_hint());
    let (inst, wit) = S.sample_random_instance_witness(&ck).unwrap();
    assert!(S.is_sat_relaxed(&ck, &inst, &wit).is_ok());
  }

  #[test]
  fn test_random_sample() {
    test_random_sample_with::<PallasEngine>();
    test_random_sample_with::<Bn256EngineKZG>();
    test_random_sample_with::<Secp256k1Engine>();
  }
}
