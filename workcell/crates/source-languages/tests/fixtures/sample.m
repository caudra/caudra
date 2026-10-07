#import <Foundation/Foundation.h>
@import CoreGraphics;

#define MAX_SHAPES 16

@protocol Drawable <NSObject>
- (void)drawInContext:(CGContextRef)context;
@end

@interface Canvas : NSObject <Drawable>
@property (nonatomic, copy) NSString *title;
- (instancetype)initWithTitle:(NSString *)title capacity:(NSUInteger)capacity;
@end

static CGFloat clamp_unit(CGFloat value) {
    return value < 0 ? 0 : (value > 1 ? 1 : value);
}

@implementation Canvas

- (instancetype)initWithTitle:(NSString *)title capacity:(NSUInteger)capacity {
    self = [super init];
    if (self) {
        _title = [title copy];
    }
    return self;
}

- (void)drawInContext:(CGContextRef)context {
    CGContextSetAlpha(context, clamp_unit(0.5));
}

@end
