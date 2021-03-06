/**
   A parallel word-frequency counting program.

   This is meant primarily to demonstrate Rust's MapReduce framework.

   It takes a list of files on the command line and outputs a list of
   words along with how many times each word is used.

*/

use std;

import option = option;
import option::some;
import option::none;
import str;
import std::treemap;
import vec;
import io;
import io::{reader_util, writer_util};

import std::time;
import u64;

import task;
import comm;
import comm::chan;
import comm::port;
import comm::recv;
import comm::send;
import comm::methods;

// These used to be in task, but they disappeard.
type joinable_task = port<()>;
fn spawn_joinable(+f: fn~()) -> joinable_task {
    let p = port();
    let c = chan(p);
    do task::spawn() |move f| {
        f();
        c.send(());
    }
    p
}

fn join(t: joinable_task) {
    t.recv()
}

fn map(&&filename: str, emit: map_reduce::putter<str, int>) {
    let f = alt io::file_reader(filename) {
      result::ok(f) { f }
      result::err(e) { fail #fmt("%?", e) }
    };

    loop {
        alt read_word(f) {
          some(w) { emit(w, 1); }
          none { break; }
        }
    }
}

fn reduce(&&word: str, get: map_reduce::getter<int>) {
    let mut count = 0;

    loop { alt get() { some(_) { count += 1; } none { break; } } }
    
    io::println(#fmt("%s\t%?", word, count));
}

mod map_reduce {
    export putter;
    export getter;
    export mapper;
    export reducer;
    export map_reduce;

    type putter<K: send, V: send> = fn(K, V);

    type mapper<K1: send, K2: send, V: send> = fn~(K1, putter<K2, V>);

    type getter<V: send> = fn() -> option<V>;

    type reducer<K: copy send, V: copy send> = fn~(K, getter<V>);

    enum ctrl_proto<K: copy send, V: copy send> {
        find_reducer(K, chan<chan<reduce_proto<V>>>),
        mapper_done
    }

    enum reduce_proto<V: copy send> { emit_val(V), done, ref, release }

    fn start_mappers<K1: copy send, K2: copy send, V: copy send>(
        map: mapper<K1, K2, V>,
        ctrl: chan<ctrl_proto<K2, V>>, inputs: ~[K1])
        -> ~[joinable_task]
    {
        let mut tasks = ~[];
        for inputs.each |i| {
            tasks += ~[spawn_joinable(|| map_task(map, ctrl, i) )];
        }
        ret tasks;
    }

    fn map_task<K1: copy send, K2: copy send, V: copy send>(
        map: mapper<K1, K2, V>,
        ctrl: chan<ctrl_proto<K2, V>>,
        input: K1)
    {
        // log(error, "map_task " + input);
        let intermediates = treemap::treemap();

        fn emit<K2: copy send, V: copy send>(
            im: treemap::treemap<K2, chan<reduce_proto<V>>>,
            ctrl: chan<ctrl_proto<K2, V>>, key: K2, val: V)
        {
            let c;
            alt treemap::find(im, key) {
              some(_c) { c = _c; }
              none {
                let p = port();
                send(ctrl, find_reducer(key, chan(p)));
                c = recv(p);
                treemap::insert(im, key, c);
                send(c, ref);
              }
            }
            send(c, emit_val(val));
        }

        map(input, {|a,b|emit(intermediates, ctrl, a, b)});

        fn finish<K: copy send, V: copy send>(_k: K, v: chan<reduce_proto<V>>)
        {
            send(v, release);
        }
        treemap::traverse(intermediates, finish);
        send(ctrl, mapper_done);
    }

    fn reduce_task<K: copy send, V: copy send>(
        reduce: reducer<K, V>, 
        key: K,
        out: chan<chan<reduce_proto<V>>>)
    {
        let p = port();

        send(out, chan(p));

        let mut ref_count = 0;
        let mut is_done = false;

        fn get<V: copy send>(p: port<reduce_proto<V>>,
                             &ref_count: int, &is_done: bool)
           -> option<V> {
            while !is_done || ref_count > 0 {
                alt recv(p) {
                  emit_val(v) {
                    // #error("received %d", v);
                    ret some(v);
                  }
                  done {
                    // #error("all done");
                    is_done = true;
                  }
                  ref { ref_count += 1; }
                  release { ref_count -= 1; }
                }
            }
            ret none;
        }

        reduce(key, || get(p, ref_count, is_done) );
    }

    fn map_reduce<K1: copy send, K2: copy send, V: copy send>(
        map: mapper<K1, K2, V>,
        reduce: reducer<K2, V>,
        inputs: ~[K1])
    {
        let ctrl = port();

        // This task becomes the master control task. It task::_spawns
        // to do the rest.

        let reducers = treemap::treemap();
        let mut tasks = start_mappers(map, chan(ctrl), inputs);
        let mut num_mappers = vec::len(inputs) as int;

        while num_mappers > 0 {
            alt recv(ctrl) {
              mapper_done {
                // #error("received mapper terminated.");
                num_mappers -= 1;
              }
              find_reducer(k, cc) {
                let c;
                // log(error, "finding reducer for " + k);
                alt treemap::find(reducers, k) {
                  some(_c) {
                    // log(error,
                    // "reusing existing reducer for " + k);
                    c = _c;
                  }
                  none {
                    // log(error, "creating new reducer for " + k);
                    let p = port();
                    let ch = chan(p);
                    let r = reduce, kk = k;
                    tasks += ~[
                        spawn_joinable(|| reduce_task(r, kk, ch) )
                    ];
                    c = recv(p);
                    treemap::insert(reducers, k, c);
                  }
                }
                send(cc, c);
              }
            }
        }

        fn finish<K: copy send, V: copy send>(_k: K, v: chan<reduce_proto<V>>)
        {
            send(v, done);
        }
        treemap::traverse(reducers, finish);

        for tasks.each |t| { join(t); }
    }
}

fn main(argv: ~[str]) {
    if vec::len(argv) < 2u {
        let out = io::stdout();

        out.write_line(#fmt["Usage: %s <filename> ...", argv[0]]);

        // FIXME (#2815): run something just to make sure the code hasn't
        // broken yet. This is the unit test mode of this program.

        ret;
    }

    let start = time::precise_time_ns();

    map_reduce::map_reduce(map, reduce, vec::slice(argv, 1u, argv.len()));
    let stop = time::precise_time_ns();

    let elapsed = (stop - start) / 1000000u64;

    log(error, "MapReduce completed in "
             + u64::str(elapsed) + "ms");
}

fn read_word(r: io::reader) -> option<str> {
    let mut w = "";

    while !r.eof() {
        let c = r.read_char();

        if is_word_char(c) {
            w += str::from_char(c);
        } else { if w != "" { ret some(w); } }
    }
    ret none;
}
fn is_word_char(c: char) -> bool {
    char::is_alphabetic(c) || char::is_digit(c) || c == '_' }
