// Tracer-bullet C-ABI shim around mosh's C++ terminal emulator.
//
// Drives the same pipeline `Terminal::Complete::act` uses internally —
// Parser::UTF8Parser -> Parser::Actions -> Emulator — but without the
// statesync/protobuf machinery `Complete` lives in. Then renders the
// framebuffer to a newline-joined plain-text grid. The point is to prove the
// cc/link plumbing (task #6), not to be a complete API.

#include <cstddef>
#include <cstdlib>
#include <cstring>
#include <string>

#include "src/terminal/parser.h"
#include "src/terminal/terminal.h"
#include "src/terminal/terminalframebuffer.h"

namespace {
struct MoshTerm
{
  Parser::UTF8Parser parser;
  Terminal::Emulator emu;
  Parser::Actions actions;

  MoshTerm( int w, int h ) : parser(), emu( w, h ), actions() {}
};
} // namespace

extern "C" {

void* mosh_term_new( int width, int height )
{
  return new MoshTerm( width, height );
}

void mosh_term_free( void* handle )
{
  delete static_cast<MoshTerm*>( handle );
}

// Feed host (server) bytes through the VT parser into the emulator.
void mosh_term_feed( void* handle, const char* data, size_t len )
{
  MoshTerm* t = static_cast<MoshTerm*>( handle );
  for ( size_t i = 0; i < len; ++i ) {
    t->parser.input( data[i], t->actions );
    for ( const Parser::ActionPointer& a : t->actions ) {
      a->act_on_terminal( &t->emu );
    }
    t->actions.clear();
  }
  // Drain host-bound responses (DSR replies, etc.); irrelevant to the tracer.
  (void)t->emu.read_octets_to_host();
}

// Render the framebuffer to a newline-joined grid. Caller frees via
// mosh_string_free. Each empty cell renders as a single space (print_grapheme).
char* mosh_term_render( void* handle )
{
  MoshTerm* t = static_cast<MoshTerm*>( handle );
  const Terminal::Framebuffer& fb = t->emu.get_fb();
  const Terminal::Framebuffer::rows_type& rows = fb.get_rows();
  std::string out;
  for ( size_t r = 0; r < rows.size(); ++r ) {
    const Terminal::Row* row = rows[r].get();
    for ( size_t c = 0; c < row->cells.size(); ++c ) {
      row->cells[c].print_grapheme( out );
    }
    if ( r + 1 < rows.size() ) {
      out.push_back( '\n' );
    }
  }
  char* buf = static_cast<char*>( std::malloc( out.size() + 1 ) );
  std::memcpy( buf, out.data(), out.size() );
  buf[out.size()] = '\0';
  return buf;
}

void mosh_string_free( char* s )
{
  std::free( s );
}

int mosh_term_width( void* handle )
{
  return static_cast<MoshTerm*>( handle )->emu.get_fb().ds.get_width();
}

int mosh_term_height( void* handle )
{
  return static_cast<MoshTerm*>( handle )->emu.get_fb().ds.get_height();
}

int mosh_term_cursor_row( void* handle )
{
  return static_cast<MoshTerm*>( handle )->emu.get_fb().ds.get_cursor_row();
}

int mosh_term_cursor_col( void* handle )
{
  return static_cast<MoshTerm*>( handle )->emu.get_fb().ds.get_cursor_col();
}

} // extern "C"
