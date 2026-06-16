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

#ifndef NETWORK_TIMING_HPP
#define NETWORK_TIMING_HPP

#include <cstdint>

/*
 * Lightweight network timing surface, extracted from network.h /
 * transportsender.h so consumers that need only the monotonic clock and the
 * ack interval -- notably the predictive-echo overlay (terminaloverlay) -- can
 * include it WITHOUT pulling in the crypto + protobuf transport stack. This is
 * what lets posh wrap the predictor behind a C-ABI shim with a light build
 * (ADR 0004; the injected-clock characterization harness). The definitions live
 * where they always did: timestamp* in network.cc, ACK_INTERVAL is a header
 * constant. network.h and transportsender.h include this header and re-export
 * these names, so every existing call site is unchanged.
 */

namespace Network {
uint64_t timestamp( void );
uint16_t timestamp16( void );
uint16_t timestamp_diff( uint16_t tsnew, uint16_t tsold );

const int ACK_INTERVAL = 3000; /* ms between empty acks */
}

#endif
