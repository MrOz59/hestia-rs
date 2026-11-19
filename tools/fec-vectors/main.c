#include <stdio.h>
#include <stdlib.h>
#include "rs.h"
int main(void) {
    int cases[][3] = {{6,3,37},{1,2,16},{10,2,32},{20,6,24}};
    reed_solomon_init();
    for (int c = 0; c < 4; c++) {
        int ds = cases[c][0], ps = cases[c][1], bs = cases[c][2];
        reed_solomon *rs = reed_solomon_new(ds, ps);
        uint8_t **sh = malloc(sizeof(uint8_t*) * (ds + ps));
        for (int i = 0; i < ds + ps; i++) {
            sh[i] = calloc(1, bs);
            if (i < ds) for (int b = 0; b < bs; b++) sh[i][b] = (uint8_t)((i * 31 + b * 7 + 1) & 0xff);
        }
        reed_solomon_encode(rs, sh, ds + ps, bs);
        printf("%d %d %d", ds, ps, bs);
        for (int j = ds; j < ds + ps; j++) { printf(" "); for (int b = 0; b < bs; b++) printf("%02x", sh[j][b]); }
        printf("\n");
    }
    return 0;
}
