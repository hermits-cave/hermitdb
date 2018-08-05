extern crate hermitdb;
extern crate tempfile;

#[macro_use]
extern crate assert_matches;

#[macro_use]
extern crate quickcheck;

use quickcheck::{Arbitrary, Gen, TestResult};

use hermitdb::crdts::{map, orswot, Map, Orswot, CmRDT};
use hermitdb::{LogReplicable, TaggedOp};
use hermitdb::memory_log;
use hermitdb::git_log;

type TActor = u8;
type TKey = u8;
type TVal = Orswot<u8, TActor>;
type TMap = Map<TKey, TVal, TActor>;
type TOp = map::Op<TKey, TVal, TActor>;

#[derive(Debug, Clone)]
struct OpVec(TActor, Vec<TOp>);

impl Arbitrary for OpVec {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        let actor = TActor::arbitrary(g);
        let num_ops: u8 = g.gen_range(0, 50);
        let mut map = TMap::new();
        let mut ops = Vec::with_capacity(num_ops as usize);
        for _ in 0..num_ops {
            let die_roll: u8 = g.gen();
            let key = g.gen();
            let op = match die_roll % 3 {
                0 => {
                    // update Orswot
                    map.update(key, map.dot(actor.clone()), |set, dot| {
                        let die_roll: u8 = g.gen();
                        let member = g.gen();
                        match die_roll % 2 {
                            0 => set.add(member, dot),
                            _ => {
                                let ctx = set.context(&member);
                                set.remove(member, ctx)
                            }
                        }
                    })
                },
                1 => {
                    // rm
                    let ctx = map.get(&key)
                        .map(|(_, c)| c)
                        .unwrap_or(hermitdb::crdts::VClock::new());
                    map.rm(key, ctx)
                },
                _ => {
                    // nop
                    map::Op::Nop
                }
            };
            map.apply(&op);
            ops.push(op);
        }
        OpVec(actor, ops)
    }

    fn shrink(&self) -> Box<Iterator<Item = Self>> {
        let mut shrunk: Vec<Self> = Vec::new();
        for i in 0..self.1.len() {
            let mut vec = self.1.clone();
            vec.remove(i);
            shrunk.push(OpVec(self.0.clone(), vec))
        }
        Box::new(shrunk.into_iter())
    }    
}

fn p2p_pull_converge<L: LogReplicable<TActor, TMap>>(
    mut a_log: L,
    mut b_log: L,
    a_ops: Vec<TOp>,
    b_ops: Vec<TOp>
) -> TMap {
    let mut a_map = TMap::new();
    let mut b_map = TMap::new();

    for op in a_ops {
        let tagged_op = a_log.commit(op).unwrap();
        assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(a_log.ack(&tagged_op), Ok(()));
    }

    for op in b_ops {
        let tagged_op = b_log.commit(op).unwrap();
        assert_eq!(b_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(b_log.ack(&tagged_op), Ok(()));
    }

    assert_matches!(b_log.pull(&a_log), Ok(_));
    assert_matches!(a_log.pull(&b_log), Ok(_));

    while let Some(tagged_op) = a_log.next().unwrap() {
        assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(a_log.ack(&tagged_op), Ok(()));
    }

    while let Some(tagged_op) = b_log.next().unwrap() {
        assert_matches!(b_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(b_log.ack(&tagged_op), Ok(()));
    }

    assert_eq!(a_map, b_map);
    a_map
}

fn centralized_converge<L: LogReplicable<TActor, TMap>>(
    mut a_log: L,
    mut b_log: L,
    mut c_log: L,
    a_ops: Vec<TOp>,
    b_ops: Vec<TOp>
) -> TMap {
    let mut a_map = TMap::new();
    let mut b_map = TMap::new();

    for op in a_ops {
        let tagged_op = a_log.commit(op).unwrap();
        assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(a_log.ack(&tagged_op), Ok(()));
    }

    for op in b_ops {
        let tagged_op = b_log.commit(op).unwrap();
        assert_eq!(b_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(b_log.ack(&tagged_op), Ok(()));
    }

    assert_matches!(a_log.push(&mut c_log), Ok(()));
    assert_matches!(b_log.push(&mut c_log), Ok(()));
    
    assert_matches!(a_log.pull(&c_log), Ok(()));
    assert_matches!(b_log.pull(&c_log), Ok(()));

    while let Some(tagged_op) = a_log.next().unwrap() {
        assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(a_log.ack(&tagged_op), Ok(()));
    }

    while let Some(tagged_op) = b_log.next().unwrap() {
        assert_matches!(b_map.apply(tagged_op.op()), Ok(()));
        assert_matches!(b_log.ack(&tagged_op), Ok(()));
    }

    assert_eq!(a_map, b_map);
    a_map
}

fn all_replication_strategies_converge<L: LogReplicable<TActor, TMap>>(
    a_pull: L, b_pull: L,
    a_central: L, b_central: L, c_central: L,
    a_ops: Vec<TOp>,
    b_ops: Vec<TOp>
) {
    let pull_map = p2p_pull_converge(a_pull, b_pull, a_ops.clone(), b_ops.clone());
    let central_map = centralized_converge(a_central, b_central, c_central, a_ops, b_ops);

    assert_eq!(pull_map, central_map);
}

fn log_preserves_order(mut log: impl LogReplicable<TActor, TMap>, ops: Vec<TOp>) {
    for op in ops.iter() {
        assert_matches!(log.commit(op.clone()), Ok(_));
    }

    for op in ops.iter() {
        let tagged_op = log.next().unwrap().unwrap();
        assert_eq!(op, tagged_op.op());
        log.ack(&tagged_op).unwrap();
    }
    assert_matches!(log.next(), Ok(None));
}

quickcheck! {
    fn prop_replication_strategies_converges_memory(a_ops: OpVec, b_ops: OpVec) -> TestResult {
        let (actor1, a_ops) = (a_ops.0, a_ops.1);
        let (actor2, b_ops) = (b_ops.0, b_ops.1);

        if actor1 == actor2 {
            return TestResult::discard();
        }

        let a_pull = memory_log::Log::new(actor1);
        let b_pull = memory_log::Log::new(actor2);
        let a_central = memory_log::Log::new(actor1);
        let b_central = memory_log::Log::new(actor2);

        // TAI: to avoid this dummy actor, consider moving the actor to the trait functions that require an actor.
        let c_central = memory_log::Log::new(0); // this actor shouldn't matter
        
        all_replication_strategies_converge(
            a_pull, b_pull,
            a_central, b_central, c_central,
            a_ops, b_ops
        );
        TestResult::from_bool(true)
    }

    fn prop_replication_strategies_converge_git(a_ops: OpVec, b_ops: OpVec) -> TestResult {
        let (actor1, a_ops) = (a_ops.0, a_ops.1);
        let (actor2, b_ops) = (b_ops.0, b_ops.1);

        if actor1 == actor2 {
            return TestResult::discard();
        }

        let a_pull_dir = tempfile::tempdir().unwrap();
        let b_pull_dir = tempfile::tempdir().unwrap();
        let a_central_dir = tempfile::tempdir().unwrap();
        let b_central_dir = tempfile::tempdir().unwrap();
        let c_central_dir = tempfile::tempdir().unwrap();
        
        let a_pull_git = hermitdb::git2::Repository::init_bare(&a_pull_dir.path()).unwrap();
        let b_pull_git = hermitdb::git2::Repository::init_bare(&b_pull_dir.path()).unwrap();
        let a_central_git = hermitdb::git2::Repository::init_bare(&a_central_dir.path()).unwrap();
        let b_central_git = hermitdb::git2::Repository::init_bare(&b_central_dir.path()).unwrap();
        let c_central_git = hermitdb::git2::Repository::init_bare(&c_central_dir.path()).unwrap();
        
        let a_pull = git_log::Log::no_auth(actor1, a_pull_git, "a_pull".into(), a_pull_dir.path().to_str().unwrap().to_string());
        let b_pull = git_log::Log::no_auth(actor2, b_pull_git, "b_pull".into(), b_pull_dir.path().to_str().unwrap().to_string());
        let a_central = git_log::Log::no_auth(actor1, a_central_git, "a_central".into(), a_central_dir.path().to_str().unwrap().to_string());
        let b_central = git_log::Log::no_auth(actor2, b_central_git, "b_central".into(), b_central_dir.path().to_str().unwrap().to_string());
        let c_central = git_log::Log::no_auth(0, c_central_git, "c_central".into(), c_central_dir.path().to_str().unwrap().to_string());
        
        all_replication_strategies_converge(
            a_pull, b_pull,
            a_central, b_central, c_central,
            a_ops, b_ops
        );
        TestResult::from_bool(true)
    }

    fn prop_log_preserves_order_memory(ops: OpVec) -> bool {
        let log: memory_log::Log<u8, TMap> = memory_log::Log::new(ops.0);
        log_preserves_order(log, ops.1);
        true
    }

    fn prop_log_preserves_order_git(ops: OpVec) -> bool {
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path();
        let log_git = hermitdb::git2::Repository::init_bare(&log_path).unwrap();
        let log_path_string = log_path.to_str().unwrap().to_string();
        let log = git_log::Log::no_auth(ops.0, log_git, "log".into(), log_path_string);;
        
        log_preserves_order(log, ops.1);

        true
    }
}

#[test]
fn test_quickcheck_1() {
    let mut a_log: memory_log::Log<u8, TMap> = memory_log::Log::new(89);
    let mut b_log: memory_log::Log<u8, TMap> = memory_log::Log::new(51);
    let mut a_map = TMap::new();
    let mut b_map = TMap::new();

    let op = map::Op::Up {
        dot: hermitdb::crdts::Dot { actor: 51, counter: 5 },
        key: 3,
        op: orswot::Op::Rm {
            context: hermitdb::crdts::VClock::new(),
            member: 21
        }
    };
    let tagged_op = b_log.commit(op).unwrap();
    assert_matches!(b_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(b_log.ack(&tagged_op), Ok(()));

    assert_matches!(b_log.pull(&a_log), Ok(()));
    assert_matches!(a_log.pull(&b_log), Ok(()));

    println!("a_log: {:#?}", a_log);
    println!("b_log: {:#?}", b_log);
    
    let tagged_op = a_log.next().unwrap().unwrap();
    assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(a_log.ack(&tagged_op), Ok(()));
    assert_matches!(a_log.next(), Ok(None));

    assert_matches!(b_log.next(), Ok(None));
    assert_eq!(a_map, b_map);
}

#[test]
fn test_quickcheck_2() {
    let mut a_log: memory_log::Log<u8, TMap> = memory_log::Log::new(89);
    let mut b_log: memory_log::Log<u8, TMap> = memory_log::Log::new(51);
    let mut a_map = TMap::new();
    let mut b_map = TMap::new();

    let op = map::Op::Rm {
        context: vec![(44, 17)].into_iter().collect(),
        key: 196
    };
    let tagged_op = b_log.commit(op).unwrap();
    
    assert_matches!(b_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(b_log.ack(&tagged_op), Ok(()));

    assert_matches!(b_log.pull(&a_log), Ok(()));
    assert_matches!(a_log.pull(&b_log), Ok(()));

    println!("a_log: {:#?}", a_log);
    println!("b_log: {:#?}", b_log);
    
    let tagged_op = a_log.next().unwrap().unwrap();
    assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(a_log.ack(&tagged_op), Ok(()));
    assert_matches!(a_log.next(), Ok(None));

    assert_matches!(b_log.next(), Ok(None));
    assert_eq!(a_map, b_map);
}

#[test]
fn test_quickcheck_3() {
    
//    let root_a: &Path = Path::new("/Users/davidrusu/hermitdb/a");
//    let root_b: &Path = Path::new("/Users/davidrusu/hermitdb/b");
//    let a_log_path: &Path = &root_a.join("db");
//    let b_log_path: &Path = &root_b.join("db");
    
    let a_log_dir = tempfile::tempdir().unwrap();
    let b_log_dir = tempfile::tempdir().unwrap();

    let a_log_path = a_log_dir.path();
    let b_log_path = b_log_dir.path();
    
    
    let a_log_git = hermitdb::git2::Repository::init_bare(&a_log_path).unwrap();
    let b_log_git = hermitdb::git2::Repository::init_bare(&b_log_path).unwrap();


    let actor1 = 1;
    let actor2 = 2;
    let mut a_log: git_log::Log<TActor, TMap> = git_log::Log::no_auth(actor1, a_log_git, "a_log".into(), a_log_path.to_str().unwrap().to_string());
    let mut b_log: git_log::Log<TActor, TMap> = git_log::Log::no_auth(actor2, b_log_git, "b_log".into(), b_log_path.to_str().unwrap().to_string());

    let mut a_map = TMap::new();
    let mut b_map = TMap::new();

    let op: TOp = map::Op::Nop;

    assert_matches!(b_log.commit(op), Ok(_));
    assert_eq!(b_log.next().unwrap().unwrap().op(), &map::Op::Nop);
    let tagged_op = b_log.next().unwrap().unwrap();
    assert_matches!(b_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(b_log.ack(&tagged_op), Ok(()));

    assert_matches!(b_log.pull(&a_log), Ok(()));
    assert_matches!(a_log.pull(&b_log), Ok(()));

    let tagged_op = a_log.next().unwrap().unwrap();
    assert_matches!(a_map.apply(tagged_op.op()), Ok(()));
    assert_matches!(a_log.ack(&tagged_op), Ok(()));
    assert_matches!(a_log.next(), Ok(None));

    assert_matches!(b_log.next(), Ok(None));
    assert_eq!(a_map, b_map);
}
