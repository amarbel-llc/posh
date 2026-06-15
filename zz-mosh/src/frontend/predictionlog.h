/*
    Mosh: the mobile shell
    Copyright 2012 Keith Winstein

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <http://www.gnu.org/licenses/>.

    In addition, as a special exception, the copyright holders give
    permission to link the code of portions of this program with the
    OpenSSL library under certain conditions as described in each
    individual source file, and distribute linked combinations including
    the two.

    You must obey the GNU General Public License in all respects for all
    of the code used other than OpenSSL. If you modify file(s) with this
    exception, you may extend this exception to your version of the
    file(s), but you are not obligated to do so. If you do not wish to do
    so, delete this exception statement from your version. If you delete
    this exception statement from all source files in the program, then
    also delete it here.
*/

#ifndef PREDICTIONLOG_HPP
#define PREDICTIONLOG_HPP

/*
   Event-level trace logging for the client-side prediction engine
   (PredictionEngine in terminaloverlay.cc). This is a study tool: a faithful
   record of every decision the predictor makes, used to reverse-engineer the
   algorithm and reproduce it in posh's predictive local echo.

   The mosh client owns a full-screen terminal, so the engine's original
   debug prints (the commented-out fprintf(stderr, ...) calls throughout
   terminaloverlay.cc) are unusable live. This routes those decision points to
   a file instead, gated by the MOSH_PREDICTION_LOG environment variable so a
   normal client build is unaffected (no file opened, near-zero cost).

   Style mirrors posh's own file logger (crates/posh/src/util.rs, gated by
   POSH_DEBUG_LOG): append-only, one line per record shaped
   "[<ts>] [predict]: <event ...>", with a 5 MiB size cap that renames the
   current file to "<path>.old" on rollover.

   The bracketed <ts> is mosh's own Network::timestamp() (a monotonic
   milliseconds clock), NOT wall-clock epoch. That is deliberate: it is the
   exact clock the engine uses for every threshold it reasons about
   (GLITCH_THRESHOLD, GLITCH_FLAG_THRESHOLD, send_interval), so deltas between
   log lines line up directly with the engine's own timing math.
*/

namespace Overlay {
/* Whether MOSH_PREDICTION_LOG named a writable path (lazily probed, cached).
   Guard expensive argument construction (e.g. Cell::debug_contents()) with
   this so it is skipped entirely in a normal build. */
bool prediction_log_enabled( void );

/* Append one printf-formatted line to the prediction log. A no-op (and does
   not touch varargs) when logging is disabled. The timestamp prefix and
   trailing newline are added automatically. */
void prediction_log( const char* fmt, ... );
}

#endif
