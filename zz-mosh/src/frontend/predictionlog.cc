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

#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

#include "src/frontend/predictionlog.h"
#include "src/network/network.h" /* Network::timestamp() */

/* The engine that drives this is single-threaded (the client overlay loop),
   so plain file-statics are sufficient; no locking, unlike posh's Mutex-wrapped
   logger (Rust idiom, not a concurrency requirement here). */
namespace {
enum LogState
{
  Untried,  /* MOSH_PREDICTION_LOG not yet probed */
  Disabled, /* unset, empty, or unopenable -> permanently inert */
  Enabled   /* file open and writable */
};

const long LOG_MAX_SIZE = 5 * 1024 * 1024; /* 5 MiB, matches posh util.rs */

LogState state = Untried;
FILE* log_file = NULL;
char* log_path = NULL;
long log_size = 0;

/* Probe the environment exactly once and open the file in append mode. */
void ensure_init( void )
{
  if ( state != Untried ) {
    return;
  }

  const char* path = getenv( "MOSH_PREDICTION_LOG" );
  if ( ( path == NULL ) || ( path[0] == '\0' ) ) {
    state = Disabled;
    return;
  }

  log_file = fopen( path, "a" );
  if ( log_file == NULL ) {
    state = Disabled;
    return;
  }

  log_path = strdup( path );
  long pos = ftell( log_file );
  log_size = ( pos > 0 ) ? pos : 0;
  state = Enabled;
}

/* Rename the current log to "<path>.old" and start a fresh one, mirroring
   posh's size-rotation behavior. Disables logging if the reopen fails. */
void rotate_log( void )
{
  if ( ( log_file == NULL ) || ( log_path == NULL ) ) {
    return;
  }

  fclose( log_file );
  log_file = NULL;

  std::string old_path = std::string( log_path ) + ".old";
  rename( log_path, old_path.c_str() );

  log_file = fopen( log_path, "a" );
  if ( log_file == NULL ) {
    state = Disabled;
    return;
  }
  log_size = 0;
}
}

bool Overlay::prediction_log_enabled( void )
{
  ensure_init();
  return state == Enabled;
}

void Overlay::prediction_log( const char* fmt, ... )
{
  ensure_init();
  if ( state != Enabled ) {
    return;
  }

  if ( log_size >= LOG_MAX_SIZE ) {
    rotate_log();
    if ( state != Enabled ) {
      return;
    }
  }

  int prefix = fprintf( log_file, "[%lu] [predict]: ", (unsigned long)Network::timestamp() );

  va_list ap;
  va_start( ap, fmt );
  int body = vfprintf( log_file, fmt, ap );
  va_end( ap );

  fputc( '\n', log_file );
  fflush( log_file );

  if ( prefix > 0 ) {
    log_size += prefix;
  }
  if ( body > 0 ) {
    log_size += body;
  }
  log_size += 1; /* newline */
}
