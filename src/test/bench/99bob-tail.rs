/* -*- mode::rust;indent-tabs-mode::nil -*-
 * Implementation of 99 Bottles of Beer
 * http://99-bottles-of-beer.net/
 */
use std;
import int;
import str;

fn main() {
    fn multiple(n: int) {
        #debug("%d bottles of beer on the wall, %d bottles of beer,", n, n);
        #debug("Take one down and pass it around, %d \
                bottles of beer on the wall.", n-1);
        #debug("");
        if n > 3 { ret multiple(n - 1); } else { ret dual(); }
    }
    fn dual() {
        #debug("2 bottles of beer on the wall, 2 bottles of beer,");
        #debug("Take one down and pass it around, \
                1 bottle of beer on the wall.");
        #debug("");
        ret single();
    }
    fn single() {
        #debug("1 bottle of beer on the wall, 1 bottle of beer,");
        #debug("Take one down and pass it around, \
                no more bottles of beer on the wall.");
        #debug("");
        ret none();
    }
    fn none() {
        #debug("No more bottles of beer on the wall, \
                no more bottles of beer,");
        #debug("Go to the store and buy some more, \
                99 bottles of beer on the wall.");
        #debug("");
    }
    multiple(99);
}
