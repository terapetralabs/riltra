# SPDX-License-Identifier: Apache-2.0
import os
from a.b import c as d
@decorator
def deco(x):
    def inner(): return x
    return inner()
async def fetch():
    await deco(1)
class K(Base):
    @staticmethod
    def s(): os.getcwd()
    async def am(self): await self.s()
f = lambda: d()
