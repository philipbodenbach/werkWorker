"""Minimal ANSI screen emulator shared by real PTY regressions."""
import codecs
import re
import unicodedata

class Screen:
    def __init__(self):
        self.rows = [[' '] * 130 for _ in range(36)]
        self.x = self.y = 0
        self.pending = ''
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith('\x1b['):
                m = re.match(r'\x1b\[([0-?]*)([ -/]*)([@-~])', self.pending)
                if not m: break
                args = [int(v) if v else 1 for v in m[1].lstrip('?').split(';')]
                code = m[3]
                if code in ['H','f']:
                    self.y = args[0]-1; self.x = (args[1] if len(args)>1 else 1)-1
                elif code == 'J' and args[0] in [2,3]: self.rows = [[' '] * 130 for _ in range(36)]
                elif code == 'C': self.x += args[0]
                elif code == 'D': self.x -= args[0]
                self.pending = self.pending[m.end():]; continue
            if self.pending == '\x1b': break
            c = self.pending[0]; self.pending = self.pending[1:]
            if c == '\r': self.x = 0
            elif c == '\n': self.y += 1
            elif c.isprintable() and 0 <= self.y < 36 and 0 <= self.x < 130:
                self.rows[self.y][self.x] = c
                self.x += 2 if unicodedata.east_asian_width(c) in ['W','F'] else 1
    def text(self): return '\n'.join(''.join(row) for row in self.rows).encode()

